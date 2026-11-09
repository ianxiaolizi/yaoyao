use super::*;

#[async_trait::async_trait]
impl AgentLoop for DefaultAgentLoop {
    async fn begin_turn(&self, ctx: &TurnContext) -> Result<TurnStart, AgentLoopError> {
        // Derived once per turn, not once per step: the settled prefix cannot
        // change while this turn is in flight, since the Scheduler appends this
        // turn's events only after the turn ends.
        let derived = self.context.derive(&ctx.session_id).await?;
        let context_revision = derived.revision;
        let context_message_count = derived.messages.len();
        let context_usage = derived.usage;
        // The engine's proposal travels with the derivation. Nothing is
        // compacted here: `begin_turn` makes no model request, and the
        // proposal is only worth acting on if the first step is about to send
        // a request.
        let compaction = derived.compaction;

        // The run's instructions lead the request. They are durable for the
        // same reason this turn's input is: the Scheduler resolved them and is
        // about to record them in `run/started`, which lands only after this
        // call returns. The uid is derived from the run, so a fold that reads
        // the log later rebuilds this exact message.
        //
        // A derivation can still carry the previous run's message; it is
        // replaced rather than followed, because a request has one system
        // message and it is the one this run recorded. `None` leaves the
        // derived history alone — a harness with no source, or a session whose
        // log was written before runs carried instructions.
        let mut derived_messages = derived.messages;
        if let Some(instructions) = &ctx.instructions {
            let message = ModelMessage::system_for_run(&ctx.run_id, instructions.text.clone());
            match derived_messages.first_mut() {
                // A summary (or the ledger beside it) is history even though
                // its role is `system`; the instructions replace the run
                // preamble, never a compaction product.
                Some(first)
                    if first.role == MessageRole::System && !first.is_compaction_output() =>
                {
                    *first = message;
                }
                _ => derived_messages.insert(0, message),
            }
        }

        // The tool set for this turn, frozen before the first request. Building
        // it once keeps every step's manifest identical — a manifest id that
        // changed between steps of one turn would be noise, not information.
        // It is built here because the capability layer's tool index is
        // generated from it.
        //
        // `plan.update` is the loop's own tool and no provider implements it,
        // but it still reaches the model through this manifest. A gateway that
        // somehow offers the name is made to lose it: the loop intercepts the
        // call before dispatch, so the provider's definition would only
        // describe a tool that can never run.
        let (manifest, tool_sets) = if self.offers_tools() {
            // What the gateway offers this model, not everything it could
            // execute: a tool every possible call would be refused is not a
            // choice, and a manifest that lists it is one more entry to read.
            let mut definitions = self.gateway.model_definitions();
            definitions.retain(|definition| {
                definition.name != plan::TOOL_NAME
                    && definition.name != state::TOOL_NAME
                    && definition.name != judgment::TOOL_NAME
            });
            // Recorded beside the manifest because the manifest itself keeps
            // only what reaches the wire; rehydration needs the declared side
            // effect, which does not. Derived before the loop's own tools are
            // appended: those are ledgers the loop answers, not calls that
            // touch the world.
            let tool_sets = ToolSets::from_definitions(&definitions);
            definitions.push(plan::tool_definition());
            // The work state is the second loop-owned ledger, on the same
            // terms: the loop answers it, so a provider's definition of the
            // name would only describe a tool that can never run.
            definitions.push(state::tool_definition());
            // The judgment tool is offered on the same terms as the plan: the
            // loop answers it itself, so a provider that claimed the name would
            // only describe a tool that can never run.
            if self.judgment_ledger {
                definitions.push(judgment::tool_definition());
            }
            (ToolManifest::from_definitions(&definitions), tool_sets)
        } else {
            (ToolManifest::empty(), ToolSets::default())
        };
        let advice = ToolAdvice::new(&tool_sets);

        // The derivation is [run preamble, ...history]. A summary or ledger is
        // history even though it is a system message, so the split stops at
        // it; a log written before runs carried instructions may have no
        // preamble.
        let preamble_len = derived_messages
            .iter()
            .take_while(|message| {
                message.role == MessageRole::System && !message.is_compaction_output()
            })
            .count();
        let mut messages: Vec<ModelMessage> = derived_messages[..preamble_len].to_vec();

        // What this run can do, before the model tries something the boundary
        // will refuse. Run-scoped: the same uid on every turn, so a cached
        // prompt can reuse it. Not history — the projection never folds it.
        //
        // Position matters for cost: instructions and capability form the
        // stable head of every request in the run, so they sit before history
        // and never move. The per-turn layers below are volatile and go after
        // history instead, where rewriting them cannot break the cached prefix.
        let context_capability = ctx.capability.as_ref().map(|facts| {
            let text = capability_text(facts, &manifest);
            messages.push(ModelMessage::capability_for_run(&ctx.run_id, text.clone()));
            text
        });

        // Settled history, exactly. The range is the derivation minus its
        // preamble: code that rewrites history (compaction) inserts at
        // `history_start` and never touches the two stable head messages.
        let history_start = messages.len();
        messages.extend(derived_messages[preamble_len..].iter().cloned());
        let history_end = messages.len();

        // Time, read off the log: a read an earlier turn made that this turn's
        // own words name again. The history is the settled prefix only, so a
        // call this turn makes later is never mistaken for an old one.
        let stale = stale_read(&derived_messages, &user_text(&ctx.user_input), &tool_sets.reads);

        // The situation is derived, not narrated: every field comes from the
        // turn context, the frozen tool set and the settled history. The
        // reading is a cue-based first pass, and the stance is selected by a
        // named rule. All three are recorded before the first request, so
        // `why` can walk from every later call back to the input that opened
        // the turn.
        let input_text = user_text(&ctx.user_input);
        let reading = reason_record::read(&input_text);
        let turn_facts = ctx.state.as_ref();
        let situation = Situation {
            input_kind: match turn_facts.map(|facts| facts.input_kind) {
                Some(TurnInputKind::Scheduled) => String::from("scheduled"),
                Some(TurnInputKind::User) | None => String::from("user"),
            },
            previous_turn: turn_facts.and_then(|facts| facts.previous_turn).map(ending_word),
            resumed: turn_facts.and_then(|facts| facts.resumed.clone()),
            queued_inputs: turn_facts.map(|facts| facts.queued_inputs).unwrap_or(0),
            max_steps: turn_facts.and_then(|facts| facts.max_steps),
            plan_done: ctx.plan.as_ref().map_or(0, |plan| {
                plan.items.iter().filter(|item| item.status == PlanItemStatus::Done).count()
            }),
            plan_total: ctx.plan.as_ref().map_or(0, |plan| plan.items.len()),
            pending_failure: ctx.pending_failure.as_ref().map(|failure| failure.tool.clone()),
            stale_read: stale.as_ref().map(|stale| stale.target.clone()),
            read_tools: tool_sets.reads.len(),
            write_tools: tool_sets.writes.len(),
            irreversible: tool_sets.irreversible.clone(),
            sandbox: ctx.capability.as_ref().map(|facts| sandbox_word(facts.sandbox).to_string()),
            network_allowed: ctx.capability.as_ref().map(|facts| facts.network_allowed),
            judgment_stage: ctx
                .judgment
                .as_ref()
                .and_then(|judgment| judgment.stage)
                .map(|stage| stage.as_str().to_string()),
        };
        let selection = reason_record::select(&situation, &reading);
        let criterion_expects_check = reading
            .criterion
            .as_deref()
            .is_some_and(|criterion| criterion.contains("test") || criterion.contains("check"));
        let mut recorder = Recorder::new(ctx.turn_id.as_str());
        let input_fact = if input_text.trim().is_empty() {
            String::from("the turn opened with no text")
        } else {
            format!("the input: {}", truncate(&input_text, 120))
        };
        let situation_node = recorder.node(
            ReasonKind::Situation,
            0,
            situation.render(),
            vec![Basis::fact(input_fact)],
            "situation/derive",
            ReasonSource::Kernel,
        );
        let reading_node = recorder.node(
            ReasonKind::Reading,
            0,
            reading.render(),
            vec![Basis::node(situation_node.id.clone())],
            reading.rule(),
            ReasonSource::Kernel,
        );
        let stance_node = recorder.node(
            ReasonKind::Stance,
            0,
            selection.render(),
            vec![Basis::node(situation_node.id.clone()), Basis::node(reading_node.id.clone())],
            selection.rule,
            ReasonSource::Kernel,
        );
        let stance_node_id = stance_node.id.clone();

        // The world around this turn, then what this turn is for. Both are
        // properties of the turn, not replies in the conversation: they sit
        // after settled history and before this turn's input, so every request
        // in this turn ends with the same live frame. They are not durable
        // history — the projection never folds them — and a caller with no
        // facts places neither layer rather than inventing a time or an empty
        // queue.
        let context_layers = ctx.state.as_ref().map(|facts| {
            let layers = ContextLayers {
                state: state_text(facts),
                task: task_text(
                    &ctx.user_input,
                    facts,
                    TaskLedger {
                        plan: ctx.plan.as_ref(),
                        workers: &ctx.workers,
                        work: ctx.work.as_ref(),
                    },
                    if self.judgment_ledger { ctx.judgment.as_ref() } else { None },
                    stale.as_ref(),
                    if self.judgment_ledger { Some(&advice) } else { None },
                    None,
                ),
            };
            messages.push(ModelMessage::state_for_turn(&ctx.turn_id, layers.state.clone()));
            messages.push(ModelMessage::task_for_turn(&ctx.turn_id, layers.task.clone()));
            layers
        });

        // Where the identity layer goes: after the task layer, before the
        // notice and this turn's input. A caller with no facts places no
        // layers, and then the identity sits after history, which is the same
        // place.
        let identity_index = messages.len();

        let capabilities = required_capabilities(&manifest);

        // The run's limits, as the caller resolved them. The wall clock is
        // measured from the moment the turn opens; a harness with no facts has
        // the default budget, where only the step count is set.
        let budget = ctx.state.as_ref().map(|facts| facts.budget.clone()).unwrap_or_default();
        let completion = ctx.state.as_ref().and_then(|facts| facts.completion.clone());
        let deadline = budget
            .wall_clock_ms
            .map(|millis| std::time::Instant::now() + std::time::Duration::from_millis(millis));

        // Routing happens once per turn, before the first request. A tier that
        // changed mid-turn would make `context_revision` and the route it was
        // chosen from impossible to reconcile, and there is nothing a step can
        // learn that the turn's shape did not already say.
        let route_request = RouteRequest {
            session_id: ctx.session_id.clone(),
            turn_id: ctx.turn_id.clone(),
            // Every turn the Scheduler can start today is a user turn. When a
            // caller can ask for summarization or review, the class arrives
            // with the turn rather than being guessed here.
            task: TaskClass::new(TaskClass::INTERACTIVE),
            context_revision,
            // The frame so far, plus this turn's input, which is built a few
            // lines below. The identity layer is not in it yet for the same
            // reason its text is not: it is what this route is about to say.
            message_count: messages.len() + 1,
            required_capabilities: capabilities.clone(),
            budget: ModelBudget { max_tokens: budget.max_output_tokens, ..ModelBudget::default() },
        };
        let route = self.router.route(&route_request);

        // Who the route put at the other end, said in the request itself. The
        // harness chose this model and the model has no way to know it; a run
        // that reports its own name from training data reports a different one.
        // It goes in before the notice is measured, so the share the notice
        // reports is the request that will actually be sent.
        if let Some(text) = identity_text(&route) {
            messages.insert(identity_index, ModelMessage::identity_for_turn(&ctx.turn_id, text));
        }

        // A standing condition of this turn, not a reply to anything. It is
        // volatile like the layers above, so it goes with them, immediately
        // before this turn's input. It is not history — the projection never
        // folds it — and the prepared request records it.
        //
        // The share it reports is measured over the whole request this step
        // will send — layers, this turn's input, and the tool definitions
        // beside them — because the settled derivation alone once read 240
        // tokens while the real request carried 69,575 (2026-10-31).
        let turn_input = ModelMessage::user_for_turn(&ctx.turn_id, user_text(&ctx.user_input));
        let request_tokens =
            budget::estimated_request_tokens(&messages, &manifest) + estimate_message(&turn_input);
        let context_notice = context_notice(&context_usage, request_tokens);
        if let Some(notice) = &context_notice {
            messages.push(ModelMessage::context_notice_for_turn(&ctx.turn_id, notice.text.clone()));
        }

        // Then this turn's input. It is durable too — the Scheduler read it out
        // of the `inbox/appended` event it is holding — but this turn's
        // `turn/started` does not exist yet, so the loop is the one that places
        // it.
        messages.push(turn_input);

        let mut events: Vec<EventDraft> = Vec::new();

        // What this turn offered, in full. The prepared requests below carry
        // the manifest id and count; only this event carries the definitions
        // themselves, so a request can be read back exactly after the process
        // that assembled it is gone. Once per turn, not once per step: the set
        // is frozen above and cannot change while the turn runs.
        let frozen_entries = serde_json::to_value(&manifest.entries)?;
        events.push(draft_turn(
            ctx,
            EventType::MODEL_TOOLS_FROZEN,
            serde_json::json!({
                "manifest_id": &manifest.manifest_id,
                "created_at": manifest.created_at,
                "entries_hash": digest::json(&frozen_entries),
                "tool_count": manifest.entries.len(),
                "entries": frozen_entries,
            }),
            Actor::Agent,
        ));

        if let Some(advisor) = &self.decisions {
            // The trace is recorded as an observation. The answer is not read:
            // in shadow mode a disagreement between the advisor and the router
            // is visible in this event, and nowhere else.
            let trace = advisor.advise_model_route(&route_request, &route).await;
            events.push(draft_turn(
                ctx,
                EventType::DECISION_TRACE,
                serde_json::json!({ "trace": trace }),
                Actor::Agent,
            ));
        }

        // The turn's opening chain: situation, reading, stance. They come after
        // the frozen tool set because the situation names what that set offers;
        // they come before the first request because the stance decides what
        // that request's task layer carries.
        for node in [&situation_node, &reading_node, &stance_node] {
            events.push(reason_event(ctx, None, node));
        }

        Ok(TurnStart {
            state: TurnState::new(LoopTurn {
                ctx: ctx.clone(),
                messages,
                capabilities,
                manifest,
                context_revision,
                context_message_count,
                context_usage,
                context_notice,
                context_layers,
                context_capability,
                ledger: None,
                usertext: None,
                failures: None,
                history_start,
                history_end,
                compaction,
                tool_sets,
                last_signature: None,
                identical_streak: 0,
                repeat_refused: false,
                checkpoint_written: 0,
                in_flight_compactions: 0,
                in_flight_newest: None,
                observed_input_tokens: None,
                judgment: ctx.judgment.clone(),
                mode: selection.mode,
                reason: recorder,
                stance_node: stance_node_id,
                stance_note: None,
                stance_switched: false,
                plan_view: ctx.plan.clone(),
                work: ctx.work.clone(),
                progress: stall::Progress::default(),
                convergence: convergence::Edits::default(),
                capability_gaps: capability::CapabilityGaps::default(),
                completion,
                reason_last_observation: None,
                criterion_expects_check,
                criterion_observed: false,
                criterion_observation: None,
                step_index: 0,
                dispatched: 0,
                // A resumed turn opens on a failure the previous turn left
                // behind; the caller read it back from the log. An ordinary
                // turn opens clean.
                pending_failure: ctx.pending_failure.as_ref().map(|pending| PendingFailure {
                    call_id: pending.call_id.clone(),
                    tool: pending.tool.clone(),
                    detail: pending.detail.clone(),
                }),
                gate_fired_for: None,
                pending_retry: None,
                retry_blocked: false,
                checked_since_failure: false,
                stop_refused: false,
                stale,
                stale_gate_fired: false,
                acted_this_turn: false,
                route,
                budget,
                deadline,
                tool_calls_made: 0,
                spent: Spent::default(),
                journal: ctx.journal.clone(),
            }),
            events,
        })
    }

    async fn step(
        &self,
        turn: &mut TurnState,
        steering: &[SteeringInput],
    ) -> Result<StepOutcome, AgentLoopError> {
        let turn = turn.get_mut::<LoopTurn>().ok_or(AgentLoopError::StateNotOwned)?;
        // A copy rather than a borrow: compaction rewrites `turn.messages`
        // mid-step, and holding a borrow of `turn.ctx` would make the whole
        // turn unavailable for that. The fields are small and read once.
        let ctx = turn.ctx.clone();
        let ctx = &ctx;
        let context_revision = turn.context_revision;
        turn.step_index += 1;
        let step_index = turn.step_index;
        let mut events: Vec<EventDraft> = Vec::new();
        let step_id = StepId::new();

        // A run whose wall clock is spent opens no further step. The check is
        // before `step/started`, so a step that provably did not happen leaves
        // no half a step in the log; the steering below is not claimed either,
        // because the turn that would have carried it has ended.
        if let Some(deadline) = turn.deadline
            && std::time::Instant::now() >= deadline
        {
            return Ok(StepOutcome {
                step_id,
                events,
                stop: Some(StopReason::WallClockExceeded),
                final_content: None,
                spent: turn.spent,
            });
        }

        // The same boundary for the token budget. The spend is only known after
        // a response settles, so the response that crosses the limit is not
        // aborted; the next step is where the run stops, and the overshoot is
        // bounded by that one request.
        if turn.token_budget_spent() {
            return Ok(StepOutcome {
                step_id,
                events,
                stop: Some(StopReason::TokenBudgetExceeded),
                final_content: None,
                spent: turn.spent,
            });
        }

        // And the boundary for a run that is going in circles: twice the
        // reminder threshold of steps without new evidence ends it as
        // `Stalled`, whatever budget is left. The check is here rather than
        // after a response because the model was told to change course one
        // step ago; a further request would spend budget on the same loop.
        if turn.stall_reached() {
            return Ok(StepOutcome {
                step_id,
                events,
                stop: Some(StopReason::Stalled),
                final_content: None,
                spent: turn.spent,
            });
        }

        // And the boundary the operator's criterion is asked at. It happens
        // before the request rather than after a response because the world
        // may already be done: a request sent past that point is spend on a
        // run that has finished, and the work the model would have done next
        // is work nobody needs. A check that does not pass costs one call and
        // the turn carries on unchanged.
        if let Some(outcome) = completion::check_at_boundary(
            self.gateway.as_ref(),
            &self.redactor,
            ctx,
            turn,
            &step_id,
            &mut events,
        )
        .await?
        {
            return Ok(outcome);
        }

        // Steering goes in before anything else this step does: it was injected
        // at the boundary this step begins at, so it precedes the request that
        // will carry it — and the step/started below.
        for input in steering {
            turn.messages.push(ModelMessage::user_for_steering(
                &ctx.turn_id,
                &input.item_id,
                input.text.clone(),
            ));
            // A steer is the person changing the turn while it runs. The kernel
            // re-reads it and switches the posture when the steer changes what
            // the turn is for; the node records why, so a switch is a decision
            // with a reason rather than a mood.
            let steer_reading = reason_record::read(&input.text);
            let target = match steer_reading.act {
                SpeechAct::Correction => Some((
                    Stance::Repair,
                    "switch/steer-correction",
                    "the user corrected the running turn",
                )),
                SpeechAct::Action if turn.mode.stance != Stance::Act => Some((
                    Stance::Act,
                    "switch/steer-request",
                    "the user steered a new request into the running turn",
                )),
                _ => None,
            };
            if let Some((stance, rule, because)) = target
                && turn.mode.stance != stance
            {
                let node = turn.reason.node(
                    ReasonKind::Stance,
                    step_index,
                    format!(
                        "stance switched: {} -> {} ({because})",
                        turn.mode.stance.as_str(),
                        stance.as_str()
                    ),
                    vec![
                        Basis::node(turn.stance_node.clone()),
                        Basis::fact(format!("user steer: {}", truncate(&input.text, 120))),
                    ],
                    rule,
                    ReasonSource::Kernel,
                );
                turn.stance_node = node.id.clone();
                enter_stance(turn, stance, because);
                events.push(reason_event(ctx, Some(&step_id), &node));
                refresh_task_layer(turn, self.judgment_ledger);
            }
        }

        {
            events.push(draft(
                ctx,
                &step_id,
                EventType::STEP_STARTED,
                serde_json::json!({}),
                Actor::Agent,
            ));

            // A compaction proposal is acted on before this step's own request,
            // so the request the model sees is already the smaller one. It is
            // taken, not read: one turn summarizes settled history at most
            // once.
            if let Some(candidate) = turn.compaction.take() {
                self.compact(turn, &step_id, candidate, "none", &mut events).await?
            }

            // The turn's own traffic has no other protection: the engine
            // compacts settled history only, and the trimmer does too. Once
            // this step's request — the whole thing, tool definitions
            // included — would spend the settle share of the window, compact
            // it — at most `MAX_IN_FLIGHT_COMPACTIONS` times, and a later
            // attempt only once the newest group has changed. The first
            // attempt cannot summarize the group that crossed the share; it
            // is the newest one and is always kept. The second attempt, after
            // the model has acted on it, is what turns that group into a
            // summary. A failed attempt counts: retrying the same messages
            // every step would spend a summary request per step without
            // changing anything.
            //
            // Measured before compaction so that a turn that is already
            // compacting does not also fire on the smaller request it just
            // produced.
            let estimated_before = budget::estimated_request_tokens(&turn.messages, &turn.manifest);
            let newest = turn.messages.last().map(|message| message.message_uid.to_string());
            if turn.in_flight_compactions < MAX_IN_FLIGHT_COMPACTIONS
                && newest.as_deref() != turn.in_flight_newest.as_deref()
                && let Some(trigger) = settlement::trigger(turn, estimated_before)
            {
                // The state settles first — no provider, no invented text. A
                // turn with no state to write from keeps the summarizer. An
                // attempt counts only when one of them did something: a step
                // that merely crossed the share has nothing to spend yet.
                if settlement::settle(turn, &step_id, trigger, &mut events)? {
                    turn.in_flight_compactions += 1;
                    turn.in_flight_newest = newest;
                } else if let Some(candidate) = settle::in_flight_candidate(turn, estimated_before)
                {
                    turn.in_flight_compactions += 1;
                    turn.in_flight_newest = newest;
                    self.compact(turn, &step_id, candidate, "settlement", &mut events).await?;
                }
            }

            // Where the run stands, after this turn's traffic and before the
            // reminder about stopping (see `work_state` for why both sit at
            // the tail and are replaced rather than repeated).
            let work_block = work_state::place_work_state(turn);
            // Ahead of the budget reminder: "you are going in circles" is the
            // more specific problem when both apply, and the budget line is the
            // last thing the request says either way.
            let stall_notice = stall::place_stall_notice(turn, &ctx.turn_id);
            // The sibling question: not "is anything new?" but "has this file
            // been checked since you last changed it?".
            let convergence_notice = convergence::place_notice(turn, &ctx.turn_id);
            // A budget checkpoint is a demand to write the state down, made at
            // the point where there is still enough budget to act on it. The
            // record of having made it is the log's, so a reader can tell a
            // run that was asked from one that volunteered.
            let checkpoint = turn.crossed_checkpoint();
            let budget_notice = budget::place_budget_notice(turn, &ctx.turn_id, checkpoint);
            if let Some(point) = checkpoint {
                events.push(budget::checkpoint_event(turn, ctx, &step_id, point));
            }

            let mut request = ModelRequest {
                session_id: ctx.session_id.clone(),
                turn_id: ctx.turn_id.clone(),
                step_id: step_id.clone(),
                // `ModelRequest` is a serialized event payload and still
                // carries the raw number; the routing request carries the
                // domain type.
                context_revision: context_revision.as_u64(),
                messages: turn.messages.clone(),
                capabilities: turn.capabilities.clone(),
                response_format: None,
                model_route: Some(turn.route.clone()),
                budget: turn.model_budget(),
                tool_manifest: turn.manifest.clone(),
            };

            // The exact ordered messages sent, as references. Their content is
            // durable elsewhere already — run instructions, this turn's layers,
            // the input item, the folded history, every tool result — and
            // repeating it on each step would make the log grow with the whole
            // context on every step. The hash is what makes a reference
            // checkable: a read-back can prove the message it found is the one
            // this request carried.
            let (message_refs, messages_hash) = message_references(&request.messages)?;

            // What this request costs, on the same estimate the context engine
            // uses. `estimated_context_tokens` above describes only the settled
            // derivation; this covers the whole request. The provider's own
            // count is recorded with the response, so the two can be compared
            // without re-deriving anything.
            let estimated_messages_tokens: usize =
                request.messages.iter().map(estimate_message).sum();
            let estimated_tool_tokens = budget::estimated_tool_tokens(&request.tool_manifest);
            let mut estimated_request_tokens = estimated_messages_tokens + estimated_tool_tokens;

            let mut attempt_id = AttemptId::new();
            let mut prepared = serde_json::json!({
                "attempt_id": attempt_id,
                "context_revision": request.context_revision,
                // `message_count` is what this request actually sends;
                // the context keys describe the derivation behind it, so a
                // trimmed history can be read back without re-deriving.
                "message_count": request.messages.len(),
                "context_message_count": turn.context_message_count,
                "dropped_message_count": turn.context_usage.dropped_messages,
                "layout_version": LAYOUT_VERSION,
                "estimated_context_tokens": turn.context_usage.estimated_tokens,
                "estimated_messages_tokens": estimated_messages_tokens,
                "estimated_tool_tokens": estimated_tool_tokens,
                "estimated_request_tokens": estimated_request_tokens,
                "input_working_set_tokens": turn.context_usage.input_working_set_tokens,
                "messages": message_refs,
                "messages_hash": messages_hash,
                "tool_manifest_id": request.tool_manifest.manifest_id,
                "tool_count": request.tool_manifest.entries.len(),
                "required_capabilities": request
                    .capabilities
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>(),
                "spent": serde_json::to_value(turn.spent)?,
                "budget_phase": turn.budget_phase(),
            });
            // Absent, not null, when no reminder was placed: a request with no
            // notice has exactly the payload it had before notices existed.
            if let Some(notice) = &turn.context_notice {
                prepared["context_notice"] = notice.to_json();
            }
            // The wrap-up reminder, on the same terms as the context notice:
            // recorded with the request so the sentence the model saw can be
            // read back without re-deriving the budget.
            if let Some(notice) = &budget_notice {
                prepared["budget_notice"] = notice.to_json();
            }
            // And the stall reminder, for the same reason: the sentence the
            // model read, and the streak it was measured from, in one record.
            if let Some(text) = &stall_notice {
                prepared["stall_notice"] = serde_json::json!({
                    "unchanged_steps": turn.progress_unchanged_steps(),
                    "stall_steps": turn.stall_steps(),
                    "text": text,
                });
            }
            // The convergence reminder, on the same terms: the path, how many
            // edits armed it, and the sentence the model read.
            if let Some(notice) = &convergence_notice {
                prepared["convergence_notice"] = serde_json::json!({
                    "path": notice.path,
                    "edits": notice.edits,
                    "text": notice.text,
                });
            }
            // The state block, on the same terms: what the model read at the
            // tail, with the verdict on whether it had written an account of
            // its own. Absent when this turn keeps no work-state ledger.
            if let Some(block) = &work_block {
                prepared["work_state"] = serde_json::json!({
                    "message_uid": format!("msg/work-state/{}", ctx.turn_id),
                    "hash": digest::json(&serde_json::Value::String(block.clone())),
                    "subjective": if turn.work.as_ref().is_some_and(|work| !work.is_empty()) {
                        "present"
                    } else {
                        "none"
                    },
                    "text": block,
                });
            }
            // Absent, not null, when the caller had no facts: a harness's
            // request keeps exactly the payload it had before layers existed.
            if let Some(layers) = &turn.context_layers {
                prepared["context_layers"] = layers.to_json(&ctx.turn_id);
            }
            // The run-scoped capability layer, recorded the same way so
            // `context inspect` can read back what the model was told about
            // the boundary without deriving it again.
            if let Some(text) = &turn.context_capability {
                prepared["context_capability"] = serde_json::json!({
                    "message_uid": format!("msg/capability/{}", ctx.run_id),
                    "text": text,
                });
            }
            // The ledger this turn's compaction wrote, if any: which message
            // carries it and a hash of exactly the text this request sent, so
            // inspect can point at the directory without re-deriving it.
            if let Some(ledger) = &turn.ledger {
                prepared["context_ledger"] = ledger.to_json();
            }
            // The user's-words record this turn's compaction wrote, on the same
            // terms: which message carries it, and a hash of the exact text.
            if let Some(usertext) = &turn.usertext {
                prepared["context_usertext"] = usertext.to_json();
            }
            if let Some(failures) = &turn.failures {
                prepared["context_failures"] = failures.to_json();
            }
            events.push(draft(
                ctx,
                &step_id,
                EventType::MODEL_REQUEST_PREPARED,
                // Kept so an output recovery can record the retry as its own
                // prepared request with the same derivation, one field added.
                prepared.clone(),
                Actor::Agent,
            ));

            let declared = self.model.capabilities();
            // Fail closed rather than let a provider silently drop a feature
            // the request depends on.
            let missing: Vec<String> = request
                .capabilities
                .iter()
                .filter(|need| !declared.iter().any(|have| have.as_str() == need.as_str()))
                .map(|need| need.as_str().to_string())
                .collect();

            events.push(draft(
                ctx,
                &step_id,
                EventType::MODEL_ATTEMPT_STARTED,
                serde_json::json!({
                    "attempt_id": attempt_id,
                    "provider": self.model.provider_id(),
                    "route": serde_json::to_value(&request.model_route)?,
                }),
                Actor::Agent,
            ));

            if !missing.is_empty() {
                events.push(draft(
                    ctx,
                    &step_id,
                    EventType::MODEL_ATTEMPT_FAILED,
                    serde_json::json!({
                        "attempt_id": attempt_id,
                        "error_kind": "capability_missing",
                        "missing_capabilities": missing,
                    }),
                    Actor::Agent,
                ));
                events.push(step_failed(ctx, &step_id));
                return Ok(StepOutcome {
                    step_id,
                    events,
                    stop: Some(StopReason::FatalError),
                    final_content: None,
                    spent: turn.spent,
                });
            }

            let sink = self.sink(ctx);
            let redactor = &self.redactor;
            // The first try. A typed context overflow is the one failure the
            // loop can repair itself; `context_recovery` owns that repair and
            // returns either the response the step continues from or the
            // outcome that ends it. Every other failure keeps its ordinary
            // fatal path.
            let first = match context_recovery::first_attempt(
                self.model.as_ref(),
                sink.as_ref(),
                turn,
                context_recovery::Attempt {
                    step_id: &step_id,
                    request: &mut request,
                    prepared: &mut prepared,
                    estimated_request_tokens: &mut estimated_request_tokens,
                    attempt_id: &mut attempt_id,
                },
                &mut events,
            )
            .await?
            {
                context_recovery::First::Response(response) => response,
                context_recovery::First::Stopped(outcome) => return Ok(outcome),
            };

            // Whether the step ended on a response that spent the whole cap
            // thinking. The verdict says so once, instead of calling a request
            // that produced no content a truncated answer.
            // A reasoning model can spend the whole completion on tokens that
            // never become content (2026-10-31: 8,000 of 8,000 output tokens
            // reasoning, `finish_reason: length`, no text). The cap, not the
            // model, ended the answer, so the step re-asks once with more room
            // — or, at the model's own ceiling, less depth. `output_recovery`
            // owns that ask, including why it happens at most once.
            let (response, output_limit) = match self
                .recover_output(
                    ctx,
                    &step_id,
                    turn,
                    &request,
                    &prepared,
                    &mut attempt_id,
                    first,
                    estimated_request_tokens,
                    &mut events,
                )
                .await?
            {
                RecoveryOutcome::Settled { response, output_limit } => (response, output_limit),
                RecoveryOutcome::Failed(stop) => {
                    return Ok(StepOutcome {
                        step_id,
                        events,
                        stop: Some(stop),
                        final_content: None,
                        spent: turn.spent,
                    });
                }
            };
            let reasoning_exhausted = output_recovery::is_reasoning_only(&response);
            // The durable copy is scrubbed: a proposal's arguments are the
            // model's, and a secret copied into them must not sit in a
            // long-lived log (§7 row 4). `response` keeps its raw arguments —
            // the live turn shows the model what it actually asked for, and
            // only the record is redacted.
            let durable = redacted_proposal(&response, redactor);
            let recorded = draft(
                ctx,
                &step_id,
                EventType::MODEL_RESPONSE_COMPLETED,
                serde_json::json!({
                    "attempt_id": attempt_id,
                    "response": serde_json::to_value(&durable)?,
                }),
                Actor::Agent,
            );
            // Kept so an action can cite the response that proposed it: the
            // model's choice is a node in the chain, not an unrecorded gap
            // between two kernel decisions.
            let response_event_id = recorded.event_id.as_str().to_string();
            events.push(recorded);

            // What this response cost, added before anything else this step
            // does, so every later boundary in the step sees it.
            budget::record_spend(turn, &response, estimated_request_tokens as u64);
            events.push(draft(
                ctx,
                &step_id,
                EventType::BUDGET_UPDATED,
                serde_json::json!({
                    "spent": serde_json::to_value(turn.spent)?,
                    "phase": turn.budget_phase(),
                    "used_percent": turn.budget_used_percent(),
                    "max_total_tokens": turn.budget.max_total_tokens,
                }),
                Actor::Agent,
            ));

            // Where this step's own messages begin. Everything pushed from here
            // on is what the step learned, and what `record_evidence` reads
            // when the step ends: the assistant turn, then its tool results.
            turn.mark_evidence_start();

            // The assistant turn — its text and any tool proposals — enters the
            // conversation before its tool results. That is the order every
            // provider expects when the history is replayed.
            turn.messages.push(ModelMessage::assistant_for_attempt(
                &attempt_id,
                response.content_blocks.clone(),
            ));

            let tool_calls = response.tool_calls();
            if tool_calls.is_empty() {
                // A retry that was only described. The reconcile said the
                // next step is to retry, and the model then answered without
                // making the call — the prose form of the same stop the gate
                // exists to catch. Refused once, like the gate: a second
                // answer is taken.
                if let Some(retry) = turn.pending_retry.take()
                    && !turn.retry_blocked
                    && turn
                        .manifest
                        .entries
                        .iter()
                        .any(|entry| entry.tool_id == judgment::TOOL_NAME)
                {
                    let text = retry_text(&retry, listing_hint(&turn.manifest));
                    turn.retry_blocked = true;
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::JUDGMENT_REQUIRED,
                        serde_json::json!({
                            "tool_name": judgment::TOOL_NAME,
                            "content": text,
                        }),
                        Actor::Agent,
                    ));
                    let uid = format!("msg/kernel/{}", ctx.turn_id);
                    match turn
                        .messages
                        .iter_mut()
                        .find(|message| message.message_uid.as_str() == uid)
                    {
                        Some(message) => {
                            *message = ModelMessage::kernel_note_for_turn(&ctx.turn_id, text);
                        }
                        None => turn
                            .messages
                            .push(ModelMessage::kernel_note_for_turn(&ctx.turn_id, text)),
                    }
                    turn.record_evidence();
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::STEP_COMPLETED,
                        serde_json::json!({}),
                        Actor::Agent,
                    ));
                    return Ok(StepOutcome {
                        step_id,
                        events,
                        stop: None,
                        final_content: None,
                        spent: turn.spent,
                    });
                }
                // The gate. A real failure left a question the model has not
                // answered; an answer now would be the measured behaviour this
                // stage exists to stop (stopped without a check). The block
                // fires once per failure: the second answer is taken, because
                // a gate that can hang a turn is a worse defect than the one
                // it is fixing.
                // Nothing is asked twice: a turn that has made a real call
                // since the failure has done what the gate exists to ask for,
                // even if it has not written the reconcile yet.
                if let Some(failure) = turn.pending_failure.clone()
                    && turn.gate_fired_for.as_deref() != Some(failure.call_id.as_str())
                    && !turn.checked_since_failure
                    && turn
                        .manifest
                        .entries
                        .iter()
                        .any(|entry| entry.tool_id == judgment::TOOL_NAME)
                {
                    let text = gate_text(&failure, listing_hint(&turn.manifest));
                    turn.gate_fired_for = Some(failure.call_id.clone());
                    let gate_basis = vec![
                        Basis::node(turn.stance_node.clone()),
                        Basis::fact(format!(
                            "`{}` failed and no call has looked at the world since",
                            failure.tool
                        )),
                    ];
                    let gate = turn.reason.node(
                        ReasonKind::Gate,
                        step_index,
                        format!(
                            "held the answer under stance {}: `{}` failed and nothing has \
                             looked since",
                            turn.mode.stance.as_str(),
                            failure.tool
                        ),
                        gate_basis,
                        "gate/failure-check",
                        ReasonSource::Kernel,
                    );
                    events.push(reason_event(ctx, Some(&step_id), &gate));
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::JUDGMENT_REQUIRED,
                        serde_json::json!({
                            "tool_name": failure.tool,
                            "provider_tool_call_id": failure.call_id,
                            "content": text,
                        }),
                        Actor::Agent,
                    ));
                    // Replace rather than append: a second gate in the same
                    // turn must not leave two notes with the same uid.
                    let uid = format!("msg/kernel/{}", ctx.turn_id);
                    match turn
                        .messages
                        .iter_mut()
                        .find(|message| message.message_uid.as_str() == uid)
                    {
                        Some(message) => {
                            *message = ModelMessage::kernel_note_for_turn(&ctx.turn_id, text);
                        }
                        None => turn
                            .messages
                            .push(ModelMessage::kernel_note_for_turn(&ctx.turn_id, text)),
                    }
                    turn.record_evidence();
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::STEP_COMPLETED,
                        serde_json::json!({}),
                        Actor::Agent,
                    ));
                    return Ok(StepOutcome {
                        step_id,
                        events,
                        stop: None,
                        final_content: None,
                        spent: turn.spent,
                    });
                }
                // The time gate. This turn's words name a read an earlier turn
                // made, and nothing has been looked at since the turn began:
                // the answer would restate a fact about then. Refused once,
                // like the others — a kernel note, not a hang.
                if self.judgment_gates
                    && let Some(stale) = turn.stale.clone()
                    && !turn.stale_gate_fired
                    && !turn.acted_this_turn
                    && turn.manifest.entries.iter().any(|entry| entry.tool_id == stale.tool)
                {
                    let text = stale_text(&stale);
                    turn.stale_gate_fired = true;
                    let gate_basis = vec![
                        Basis::node(turn.stance_node.clone()),
                        Basis::fact(format!(
                            "this turn names `{}` and has made no call yet",
                            stale.target
                        )),
                    ];
                    let gate = turn.reason.node(
                        ReasonKind::Gate,
                        step_index,
                        format!(
                            "held the answer under stance {}: `{}` was read in an earlier \
                             turn, not in this one",
                            turn.mode.stance.as_str(),
                            stale.target
                        ),
                        gate_basis,
                        "gate/stale-read",
                        ReasonSource::Kernel,
                    );
                    events.push(reason_event(ctx, Some(&step_id), &gate));
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::JUDGMENT_REQUIRED,
                        serde_json::json!({
                            "tool_name": stale.tool,
                            "content": text,
                        }),
                        Actor::Agent,
                    ));
                    let uid = format!("msg/kernel/{}", ctx.turn_id);
                    match turn
                        .messages
                        .iter_mut()
                        .find(|message| message.message_uid.as_str() == uid)
                    {
                        Some(message) => {
                            *message = ModelMessage::kernel_note_for_turn(&ctx.turn_id, text);
                        }
                        None => turn
                            .messages
                            .push(ModelMessage::kernel_note_for_turn(&ctx.turn_id, text)),
                    }
                    turn.record_evidence();
                    events.push(draft(
                        ctx,
                        &step_id,
                        EventType::STEP_COMPLETED,
                        serde_json::json!({}),
                        Actor::Agent,
                    ));
                    return Ok(StepOutcome {
                        step_id,
                        events,
                        stop: None,
                        final_content: None,
                        spent: turn.spent,
                    });
                }
                // The answer is the end of the derivation: it rests on the
                // last observation this turn made (if any) and on how the
                // request was read. When a check the request named has
                // completed, the stop names that observation rather than
                // treating the model's prose as its own evidence.
                let answer_text = response.text_content();
                let mut verdict_basis = Vec::new();
                // A switched posture is the immediate reason the answer is this
                // one, so it is the first edge. The observation it rests on
                // follows; both are the same failed observation in a repair
                // turn, and the renderer says so rather than printing it twice.
                if turn.stance_switched {
                    verdict_basis.push(Basis::node(turn.stance_node.clone()));
                }
                // A criterion-observed stop rests on the check itself. Citing
                // whichever call came last would put a read under a claim about
                // a check, and a reader following the verdict's edges would
                // never reach the observation the rule names.
                let rests_on = if turn.criterion_observed {
                    turn.criterion_observation.as_ref().or(turn.reason_last_observation.as_ref())
                } else {
                    turn.reason_last_observation.as_ref()
                };
                match rests_on {
                    Some(observation) => verdict_basis.push(Basis::node(observation.clone())),
                    None if !turn.stance_switched => {
                        verdict_basis.push(Basis::node(turn.stance_node.clone()));
                    }
                    None => {}
                }
                // An action the turn only talked about is a fact, not a tone:
                // the reading named an action, the posture stayed act, and no
                // call was dispatched. Naming it on the verdict lets `why` say
                // so and the evaluation count it without reading prose. A
                // response the provider cut off at the output limit is not an
                // answer at all, and the verdict says that instead.
                let read = verdict::answer(
                    &answer_text,
                    &verdict::Facts {
                        halt: verdict::Halt::of(&response),
                        output_limit,
                        undispatched_calls: 0,
                        unmet_action: turn.dispatched == 0 && turn.mode.stance == Stance::Act,
                        criterion_observed: turn.criterion_observed,
                        repair_after_failure: turn.stance_switched
                            && turn.mode.stance == Stance::Repair,
                        reasoning_exhausted,
                    },
                );
                let verdict = turn.reason.node(
                    ReasonKind::Verdict,
                    step_index,
                    read.claim,
                    verdict_basis,
                    read.rule,
                    ReasonSource::Model,
                );
                events.push(reason_event(ctx, Some(&step_id), &verdict));
                events.push(draft(
                    ctx,
                    &step_id,
                    EventType::STEP_COMPLETED,
                    serde_json::json!({}),
                    Actor::Agent,
                ));
                return Ok(StepOutcome {
                    step_id,
                    events,
                    stop: Some(read.stop),
                    final_content: Some(answer_text),
                    spent: turn.spent,
                });
            }

            // A response the provider ended before the model finished is not a
            // plan either: the calls in it are the beginning of one (the last
            // call's arguments may never have arrived, and the model may have
            // meant more), so none is dispatched and every one is answered as a
            // call that provably did not run (CONTRACT §7 #132).
            if let Some(halt) = verdict::Halt::of(&response) {
                let text = response.text_content();
                let calls = tool_calls.len() as u32;
                let read = verdict::answer(
                    &text,
                    &verdict::Facts::halted(halt, output_limit, calls, reasoning_exhausted),
                );
                let verdict = turn.reason.node(
                    ReasonKind::Verdict,
                    step_index,
                    read.claim,
                    vec![
                        Basis::node(turn.stance_node.clone()),
                        Basis::event(response_event_id.clone()),
                    ],
                    read.rule,
                    ReasonSource::Model,
                );
                events.push(reason_event(ctx, Some(&step_id), &verdict));
                refusals::refuse_undispatched(
                    ctx,
                    &step_id,
                    turn,
                    &mut events,
                    &tool_calls,
                    &read.stop,
                    output_limit,
                );
                events.push(draft(
                    ctx,
                    &step_id,
                    EventType::STEP_COMPLETED,
                    serde_json::json!({}),
                    Actor::Agent,
                ));
                return Ok(StepOutcome {
                    step_id,
                    events,
                    stop: Some(read.stop),
                    final_content: Some(text),
                    spent: turn.spent,
                });
            }

            // The repeat gate. A third identical call is not a retry: after
            // two runs the world has already answered. The second is allowed,
            // because a transient failure deserves exactly one unchanged
            // retry (`transient`); the third is refused once, so a model with
            // no other idea is not left looping until the step budget ends.
            if self.judgment_gates
                && !turn.repeat_refused
                && turn.identical_streak >= 2
                && let Some(call) = tool_calls.iter().find(|call| {
                    turn.last_signature.as_deref().is_some_and(|last| {
                        last == call_signature(&call.tool_name, &call.arguments).as_str()
                    })
                })
            {
                let text = repeat_text(&call.tool_name, turn.identical_streak);
                let trigger = call.tool_name.clone();
                turn.repeat_refused = true;
                let gate_basis = vec![
                    Basis::node(turn.stance_node.clone()),
                    Basis::fact(format!(
                        "`{}` has run {} times in a row with byte-identical arguments",
                        call.tool_name, turn.identical_streak
                    )),
                ];
                let gate = turn.reason.node(
                    ReasonKind::Gate,
                    step_index,
                    format!(
                        "held `{}` under stance {}: the same call already ran {} times in a row",
                        call.tool_name,
                        turn.mode.stance.as_str(),
                        turn.identical_streak
                    ),
                    gate_basis,
                    "gate/repeat-call",
                    ReasonSource::Kernel,
                );
                events.push(reason_event(ctx, Some(&step_id), &gate));
                // The same cycle the dispatch path records, for the case where
                // the gate stops it before it runs (see `cycles`).
                events.push(cycles::held(ctx, &step_id, call, turn.identical_streak + 1));
                events.push(draft(
                    ctx,
                    &step_id,
                    EventType::JUDGMENT_REQUIRED,
                    serde_json::json!({
                        "tool_name": call.tool_name,
                        "provider_tool_call_id": call.tool_call_id,
                        "content": text,
                    }),
                    Actor::Agent,
                ));
                let contents: Vec<String> = tool_calls
                    .iter()
                    .map(|call| {
                        if turn.last_signature.as_deref()
                            == Some(call_signature(&call.tool_name, &call.arguments).as_str())
                        {
                            repeat_text(&call.tool_name, turn.identical_streak)
                        } else {
                            format!(
                                "not executed: this step was held because `{trigger}` was about \
                                 to run again with the same arguments."
                            )
                        }
                    })
                    .collect();
                hold_step(ctx, &step_id, turn, &mut events, &tool_calls, &contents);
                turn.record_held_step();
                events.push(draft(
                    ctx,
                    &step_id,
                    EventType::STEP_COMPLETED,
                    serde_json::json!({}),
                    Actor::Agent,
                ));
                return Ok(StepOutcome {
                    step_id,
                    events,
                    stop: None,
                    final_content: None,
                    spent: turn.spent,
                });
            }

            // Where, if anywhere, a run limit stopped this batch. The calls
            // from that index on are answered as never dispatched below: a
            // provider response's calls all need an answer, and a budget stop
            // is not a tool failure.
            let mut budget_stop: Option<(StopReason, usize)> = None;
            for (index, call) in tool_calls.iter().enumerate() {
                if let Some(deadline) = turn.deadline
                    && std::time::Instant::now() >= deadline
                {
                    budget_stop = Some((StopReason::WallClockExceeded, index));
                    break;
                }
                if let Some(limit) = turn.budget.max_tool_calls
                    && turn.tool_calls_made >= limit
                {
                    budget_stop = Some((StopReason::MaxToolCalls, index));
                    break;
                }
                turn.tool_calls_made += 1;
                // The arguments are written scrubbed: the log outlives the run
                // and is read back on resume, so what is recorded must not be
                // the secret the model happened to include (§7 row 4). The
                // invocation below still runs with the raw arguments — only the
                // record is redacted.
                let (arguments, redaction) = redactor.redact_value(&call.arguments);
                // The call is a decision, so it gets a node before it is
                // dispatched. Its bases are the stance it was made under and
                // the response that proposed it: the kernel did not choose the
                // tool, but it can say which posture the choice was made in and
                // where the proposal came from.
                let action_basis = vec![
                    Basis::node(turn.stance_node.clone()),
                    Basis::event(response_event_id.clone()),
                ];
                let action = turn.reason.node(
                    ReasonKind::Action,
                    step_index,
                    format!("{} {}", call.tool_name, summarize_arguments(&arguments)),
                    action_basis,
                    "action/dispatch",
                    ReasonSource::Model,
                );
                let action_id = action.id.clone();
                turn.dispatched += 1;
                events.push(reason_event(ctx, Some(&step_id), &action));
                // Which side answers this call, in the same vocabulary the
                // branches below use: the loop answers its own ledgers, and
                // everything else is the gateway's — including a name no
                // provider offered, which the gateway refuses.
                let loop_owned = call.tool_name == plan::TOOL_NAME
                    || call.tool_name == state::TOOL_NAME
                    || (self.judgment_ledger && call.tool_name == judgment::TOOL_NAME);
                let mut payload = serde_json::json!({
                    "tool_name": call.tool_name,
                    "provider_tool_call_id": call.tool_call_id,
                    "source": tool_source(&call.tool_name, loop_owned, &turn.manifest),
                    "arguments": arguments,
                });
                if redaction.redacted || !redaction.warnings.is_empty() {
                    // Same convention as `TOOL_RESULT_REDACTED`: a reader can
                    // tell that what it sees is not the whole of what was asked
                    // — and, when only `warnings` is non-empty, that it *is*
                    // whole and something in it looked like a secret.
                    payload["redaction"] = serde_json::json!({
                        "redacted": redaction.redacted,
                        "patterns": redaction.patterns,
                        "warnings": redaction.warnings,
                    });
                }
                events.push(draft(ctx, &step_id, EventType::TOOL_STARTED, payload, Actor::Tool));

                // `plan.update` is the loop's own tool: no provider implements
                // it, and the gateway is never asked. The ledger is written by
                // the same event the log keeps, so what the next step shows is
                // what the log says. A refused plan records no event — the
                // model gets the reason and may try again.
                if call.tool_name == plan::TOOL_NAME {
                    let (status, event_type, content) = match Plan::from_arguments(&call.arguments)
                    {
                        Ok(plan) => {
                            // The ledger is model-authored text and the log
                            // outlives the run, so it is stored scrubbed
                            // like the arguments above — and the model is
                            // handed the same scrubbed plan, so the message
                            // it sees next step is the one the log holds.
                            let (value, redaction) =
                                redactor.redact_value(&serde_json::to_value(&plan)?);
                            let plan: Plan = serde_json::from_value(value)?;
                            let mut payload = serde_json::json!({
                                "provider_tool_call_id": call.tool_call_id,
                                "plan": &plan,
                            });
                            if redaction.redacted || !redaction.warnings.is_empty() {
                                payload["redaction"] = serde_json::json!({
                                    "redacted": redaction.redacted,
                                    "patterns": redaction.patterns,
                                    "warnings": redaction.warnings,
                                });
                            }
                            events.push(draft(
                                ctx,
                                &step_id,
                                EventType::PLAN_UPDATED,
                                payload,
                                Actor::Agent,
                            ));
                            // The plan is part of the task layer this turn
                            // already placed, so that message is rewritten
                            // in place: the next step of this turn reads
                            // it without waiting for a new turn to open.
                            turn.plan_view = Some(plan.clone());
                            refresh_task_layer(turn, self.judgment_ledger);
                            let content = format!(
                                "plan recorded: {} step(s), {} fact(s)",
                                plan.items.len(),
                                plan.facts.len()
                            );
                            (ToolStatus::Completed, EventType::TOOL_COMPLETED, content)
                        }
                        Err(error) => (
                            ToolStatus::Failed,
                            EventType::TOOL_FAILED,
                            format!("plan not recorded: {error}"),
                        ),
                    };
                    answer_loop_call(
                        turn,
                        ctx,
                        &step_id,
                        step_index,
                        LoopCallAnswer { call, action_id: &action_id, status, event_type, content },
                        &mut events,
                    );
                    continue;
                }

                // `state.update` is the loop's third own tool, and with the
                // plan it is the pair a long run leans on: the plan says what
                // was intended, this says where the work actually stands. A
                // refused state records no event — the model gets the reason
                // and may try again.
                if call.tool_name == state::TOOL_NAME {
                    work_state::answer_update(
                        turn,
                        ctx,
                        &step_id,
                        step_index,
                        call,
                        &action_id,
                        redactor,
                        &mut events,
                    )?;
                    continue;
                }

                // `judgment.update` is the loop's second own tool. One call
                // records one stage; the loop merges it into the running
                // record and records the merged snapshot, so the task layer
                // and a later fold read the same chain. A reconcile must name
                // the call that actually failed — a reconcile of something
                // else is refused with the tool it should have named.
                if self.judgment_ledger && call.tool_name == judgment::TOOL_NAME {
                    // `reviewed` names a fact the loop already has: the call
                    // that just failed. The model may send it, but a reconcile
                    // that leaves it out is filled in rather than refused —
                    // making a weak model restate something the kernel knows
                    // only spends a call.
                    let mut arguments = call.arguments.clone();
                    if arguments.is_object()
                        && arguments.get("stage").and_then(|value| value.as_str())
                            == Some(JudgmentStage::Reconcile.as_str())
                        && arguments
                            .get("reviewed")
                            .and_then(|value| value.as_str())
                            .is_none_or(|reviewed| reviewed.trim().is_empty())
                        && let Some(pending) = turn.pending_failure.as_ref()
                    {
                        arguments["reviewed"] = serde_json::json!(reviewed_from(pending));
                    }
                    let accepted: Result<String, String> =
                        match Judgment::from_arguments(&arguments) {
                            Err(error) => Err(format!("judgment not recorded: {error}")),
                            Ok(update) => {
                                // Refused once per turn, like the gate: the
                                // invariant is stated, and a model that still
                                // stops has been given its chance rather than
                                // being held in a loop. What it owes the check
                                // to is either the failure it is reconciling
                                // or the retry it promised and has not made.
                                let outstanding = turn
                                    .pending_failure
                                    .as_ref()
                                    .map(|pending| pending.tool.clone())
                                    .or_else(|| turn.pending_retry.clone());
                                let stop_needs_check = update.stage
                                    == Some(JudgmentStage::Reconcile)
                                    && update.stop
                                    && !turn.checked_since_failure
                                    && !turn.stop_refused
                                    && outstanding.is_some();
                                let refusal = turn.pending_failure.as_ref().and_then(|pending| {
                                    if update.stage != Some(JudgmentStage::Reconcile) {
                                        return None;
                                    }
                                    if !update.reconciles(&pending.tool) {
                                        return Some(format!(
                                            "judgment not recorded: the last failure was \
                                                 `{}` ({}); `reviewed` must name that tool, \
                                                 for example `{} ...`",
                                            pending.tool, pending.detail, pending.tool
                                        ));
                                    }
                                    None
                                });
                                // A stop is a decision taken after a check.
                                // Stopping first and looking never is the
                                // silent give-up in a different shape.
                                let refusal = refusal.or_else(|| {
                                    if !stop_needs_check {
                                        return None;
                                    }
                                    Some(format!(
                                        "judgment not recorded: a stop needs a check — no tool \
                                         has been called since `{}` left something open. Make \
                                         one call that looks ({}), then record reconcile again \
                                         with `stop` true if it still does not settle it.",
                                        outstanding.clone().unwrap_or_default(),
                                        listing_hint(&turn.manifest),
                                    ))
                                });
                                match refusal {
                                    Some(text) => {
                                        if stop_needs_check {
                                            turn.stop_refused = true;
                                        }
                                        Err(text)
                                    }
                                    None => {
                                        let merged = match turn.judgment.as_ref() {
                                            Some(current) => current.merged_with(&update),
                                            None => update.clone(),
                                        };
                                        // The ledger is model-authored text
                                        // and the log outlives the run, so it
                                        // is stored scrubbed like the plan —
                                        // and the model is handed the same
                                        // scrubbed record next step.
                                        let (value, redaction) =
                                            redactor.redact_value(&serde_json::to_value(&merged)?);
                                        let merged: Judgment = serde_json::from_value(value)?;
                                        // The task layer is rebuilt from this
                                        // record below, so it must be the
                                        // scrubbed copy: the model must see
                                        // exactly what the log keeps.
                                        turn.judgment = Some(merged.clone());
                                        let mut payload = serde_json::json!({
                                            "provider_tool_call_id": call.tool_call_id,
                                            "judgment": &merged,
                                        });
                                        if redaction.redacted || !redaction.warnings.is_empty() {
                                            payload["redaction"] = serde_json::json!({
                                                "redacted": redaction.redacted,
                                                "patterns": redaction.patterns,
                                                "warnings": redaction.warnings,
                                            });
                                        }
                                        events.push(draft(
                                            ctx,
                                            &step_id,
                                            EventType::JUDGMENT_UPDATED,
                                            payload,
                                            Actor::Agent,
                                        ));
                                        if update.stage == Some(JudgmentStage::Reconcile) {
                                            turn.pending_failure = None;
                                            // The record has answered the
                                            // failure; the note asking the next
                                            // step to repair it is spent. The
                                            // posture itself does not return.
                                            turn.stance_note = None;
                                            // A retry is a promise until a
                                            // tool call makes it; the answer
                                            // gate holds the turn to it. The
                                            // promise is also a demand for a
                                            // check: a stop recorded after it
                                            // without a call is not taken.
                                            turn.pending_retry = if update.stop {
                                                None
                                            } else {
                                                Some(update.next.clone())
                                            };
                                            if turn.pending_retry.is_some() {
                                                turn.checked_since_failure = false;
                                            }
                                        }
                                        refresh_task_layer(turn, self.judgment_ledger);
                                        let stage =
                                            update.stage.map(|stage| stage.as_str()).unwrap_or("");
                                        Ok(format!("judgment recorded: {stage}"))
                                    }
                                }
                            }
                        };
                    let status =
                        if accepted.is_ok() { ToolStatus::Completed } else { ToolStatus::Failed };
                    let content = match accepted {
                        Ok(text) | Err(text) => text,
                    };
                    let result_event = draft(
                        ctx,
                        &step_id,
                        if status == ToolStatus::Completed {
                            EventType::TOOL_COMPLETED
                        } else {
                            EventType::TOOL_FAILED
                        },
                        serde_json::json!({
                            "tool_name": call.tool_name,
                            "provider_tool_call_id": call.tool_call_id,
                            "status": status,
                            "content": &content,
                        }),
                        Actor::Tool,
                    );
                    let result_event_id = result_event.event_id.as_str().to_string();
                    events.push(result_event);
                    let observation = turn.reason.node(
                        ReasonKind::Observation,
                        step_index,
                        format!("{} {}: {}", call.tool_name, status_word(status), content),
                        vec![Basis::node(action_id.clone()), Basis::event(result_event_id)],
                        "observation/result",
                        ReasonSource::World,
                    );
                    turn.reason_last_observation = Some(observation.id.clone());
                    events.push(reason_event(ctx, Some(&step_id), &observation));
                    turn.messages.push(ModelMessage::tool_for_call(
                        &step_id,
                        call.tool_call_id.clone(),
                        call.tool_name.clone(),
                        content,
                        status != ToolStatus::Completed,
                    ));
                    continue;
                }

                // A real tool call is the retry the reconcile promised. The
                // ledgers above are not: writing a record is not checking the
                // world, so they leave `pending_retry` standing. The flags are
                // set where the call actually ran, not here: a call the gateway
                // refused never looked at anything, and counting it as a check
                // would let a stop rest on a decision rather than an
                // observation.
                // Everything this step has produced so far becomes durable
                // before the call can change the world: the step that opened,
                // the model's answer, the reasoning node for this decision, and
                // the `tool/started` that names it. A process that dies after
                // this line leaves a dispatched call with no outcome, which
                // recovery marks `tool/outcome_unknown`; one that dies before
                // it leaves a call that provably never left. Both are answers
                // the next attempt can act on, and neither exists when the
                // whole step is written only after it ends.
                let journal = turn.journal.clone();
                journal.commit(std::mem::take(&mut events)).await?;

                let invocation = ToolInvocation::from_tool_call(
                    call.tool_call_id.clone(),
                    call.tool_name.clone(),
                    call.arguments.clone(),
                )
                .with_authorization_subject(ctx.authorization_subject.clone());
                match self.gateway.execute(&ctx.session_id, &invocation).await {
                    Ok(result) => {
                        // The call ran. It is the retry a reconcile promised,
                        // the check a stop may rest on, and newer than the
                        // earlier turn the task layer warned about.
                        turn.pending_retry = None;
                        turn.checked_since_failure = true;
                        turn.acted_this_turn = true;
                        turn.stale = None;
                        // The repeat gate's bookkeeping, and the cycle record
                        // when this run is the third of a streak (`cycles`).
                        if let Some(cycle) = cycles::note_run(turn, ctx, &step_id, call) {
                            events.push(cycle);
                        }
                        // What happened to the result on its way here, recorded
                        // before the result itself. These are the facts a reader
                        // needs to know that what it is looking at may not be
                        // everything: content that was cut, secrets that were
                        // removed, and where the whole of it went.
                        if !result.artifacts.is_empty() {
                            events.push(draft(
                                ctx,
                                &step_id,
                                EventType::TOOL_RESULT_ARTIFACT_OFFLOADED,
                                serde_json::json!({
                                    "tool_name": call.tool_name,
                                    "provider_tool_call_id": call.tool_call_id,
                                    "artifact_refs": artifact_refs(&result),
                                }),
                                Actor::Tool,
                            ));
                        }
                        if result.truncated {
                            events.push(draft(
                                ctx,
                                &step_id,
                                EventType::TOOL_RESULT_TRUNCATED,
                                serde_json::json!({
                                    "tool_name": call.tool_name,
                                    "provider_tool_call_id": call.tool_call_id,
                                }),
                                Actor::Tool,
                            ));
                        }
                        if result.redaction_metadata.redacted {
                            events.push(draft(
                                ctx,
                                &step_id,
                                EventType::TOOL_RESULT_REDACTED,
                                serde_json::json!({
                                    "tool_name": call.tool_name,
                                    "provider_tool_call_id": call.tool_call_id,
                                    "patterns": result.redaction_metadata.patterns,
                                }),
                                Actor::Tool,
                            ));
                        }
                        if !result.redaction_metadata.warnings.is_empty() {
                            events.push(draft(
                                ctx,
                                &step_id,
                                EventType::TOOL_RESULT_HIGH_ENTROPY,
                                serde_json::json!({
                                    "tool_name": call.tool_name,
                                    "provider_tool_call_id": call.tool_call_id,
                                    "warnings": result.redaction_metadata.warnings,
                                }),
                                Actor::Tool,
                            ));
                        }

                        // Read before the payload takes ownership: the chain's
                        // observation needs the first line of what the world
                        // said, and the result itself can be large.
                        let outcome_line = first_line(&result.content);
                        let mut payload = serde_json::json!({
                            "tool_name": call.tool_name,
                            "provider_tool_call_id": call.tool_call_id,
                            "status": result.status,
                            "content": result.content,
                            // Structured, not a hash interpolated into prose: a
                            // reader that wants the bytes needs the locator, and
                            // the model needs the same thing the log keeps.
                            "artifact_refs": artifact_refs(&result),
                            "truncated": result.truncated,
                        });
                        // Facts the tool stated, kept beside the text so a
                        // reader does not have to parse prose that only looks
                        // structured. Omitted when nothing is known, which
                        // leaves a result without facts byte-identical to the
                        // payload this loop wrote before the field existed.
                        if !result.facts.is_empty() {
                            payload["facts"] = serde_json::to_value(&result.facts)?;
                        }
                        // Where the text came from, when it is not this
                        // session's own work: the projection shows untrusted
                        // results as data. Omitted for a trusted result, which
                        // leaves the payload unchanged from before the field.
                        if result.trust == TrustLevel::Untrusted {
                            payload["trust"] = serde_json::json!(result.trust);
                        }
                        // The gate's own statement of what it decided, beside
                        // the result it decided about: a reader can see which
                        // capabilities the call ran under without re-deriving
                        // the rules from the rest of the log.
                        if let Some(policy) = &result.policy {
                            payload["policy"] = serde_json::to_value(policy)?;
                        }
                        let event_type = match result.status {
                            ToolStatus::Completed => EventType::TOOL_COMPLETED,
                            ToolStatus::Failed => EventType::TOOL_FAILED,
                            ToolStatus::Cancelled => EventType::TOOL_CANCELLED,
                        };
                        let result_event = draft(ctx, &step_id, event_type, payload, Actor::Tool);
                        let result_event_id = result_event.event_id.as_str().to_string();
                        events.push(result_event);
                        // What the world said, tied to the call that asked. A
                        // result whose call is missing from the chain would be
                        // an observation with no question behind it.
                        let observation = turn.reason.node(
                            ReasonKind::Observation,
                            step_index,
                            format!(
                                "{} {}: {}",
                                call.tool_name,
                                status_word(result.status),
                                outcome_line
                            ),
                            vec![Basis::node(action_id.clone()), Basis::event(result_event_id)],
                            "observation/result",
                            ReasonSource::World,
                        );
                        // A criterion is satisfied by a check *running*, so the
                        // call has to carry a command that names one. A read
                        // whose path contains "test" is looking at the check,
                        // not performing it: counting it would let a request to
                        // explain a test file end as "the check observed".
                        let command_text = check_command(call);
                        let ran_a_check = command_text.as_deref().is_some_and(is_check_command);
                        let check_like = turn.criterion_expects_check && ran_a_check;
                        if check_like && result.status == ToolStatus::Completed {
                            turn.criterion_observed = true;
                            turn.criterion_observation = Some(observation.id.clone());
                        }
                        convergence::note_result(turn, call, ran_a_check, result.status);
                        turn.reason_last_observation = Some(observation.id.clone());
                        events.push(reason_event(ctx, Some(&step_id), &observation));
                        let is_error = result.status != ToolStatus::Completed;
                        if is_error {
                            // A real failure is one the next step could act
                            // on: the gate will not let the turn end on an
                            // answer until the record reconciles this call.
                            // The call that failed does not count as the check
                            // a stop needs — it is the question, not an answer.
                            //
                            // It also takes back any earlier check: the work
                            // kept changing after the check ran, so the check
                            // no longer covers what the answer would rest on.
                            turn.criterion_observed = false;
                            turn.criterion_observation = None;
                            turn.checked_since_failure = false;
                            turn.pending_failure = Some(PendingFailure {
                                call_id: call.tool_call_id.clone(),
                                tool: call.tool_name.clone(),
                                detail: first_line(&result.content),
                            });
                            // A failure is the one event that changes what the
                            // turn is for. The posture switches to repair, and
                            // the next request's task layer says why; the node
                            // points at the failed observation, so a reader can
                            // see what caused the switch.
                            if turn.mode.stance != Stance::Repair {
                                let node = turn.reason.node(
                                    ReasonKind::Stance,
                                    step_index,
                                    format!(
                                        "stance switched: {} -> repair (`{}` failed)",
                                        turn.mode.stance.as_str(),
                                        call.tool_name
                                    ),
                                    vec![
                                        Basis::node(observation.id.clone()),
                                        Basis::node(turn.stance_node.clone()),
                                    ],
                                    "switch/failure-repair",
                                    ReasonSource::Kernel,
                                );
                                turn.stance_node = node.id.clone();
                                enter_stance(turn, Stance::Repair, "a call failed");
                                events.push(reason_event(ctx, Some(&step_id), &node));
                                refresh_task_layer(turn, self.judgment_ledger);
                            } else {
                                // Already repairing: no second switch node —
                                // the posture did not change — but the advice
                                // must name the failure that just happened, not
                                // the one that first opened the posture.
                                turn.stance_note = Some(repair_note(
                                    turn.pending_failure.as_ref(),
                                    "a call failed",
                                ));
                                refresh_task_layer(turn, self.judgment_ledger);
                            }
                        } else if turn.mode.stance == Stance::Repair
                            && turn
                                .pending_failure
                                .as_ref()
                                .is_some_and(|pending| pending.tool == call.tool_name)
                        {
                            // The call that went wrong has now gone right. The
                            // posture stays repair (a stance does not return
                            // inside a turn, #77), but the note that told the
                            // next step to repair it is no longer true.
                            turn.stance_note = None;
                            refresh_task_layer(turn, self.judgment_ledger);
                        }
                        turn.messages.push(ModelMessage::tool_for_call(
                            &step_id,
                            call.tool_call_id.clone(),
                            call.tool_name.clone(),
                            result.content,
                            is_error,
                        ));
                    }
                    Err(e) => {
                        if let ToolGatewayError::ApprovalPending { approval_id, .. } = &e {
                            // The call did not run, so it has no outcome of its
                            // own — but it still gets a result. The turn ends
                            // here (this loop returns `NeedsUserInput`), so the
                            // message is never sent to a provider; it is pushed
                            // to keep the turn's own message list complete and
                            // to mirror the result the log records for this
                            // step. "Nothing was executed" is a fact worth
                            // recording rather than a gap.
                            let content = format!(
                                "approval required: {approval_id}; the tool was not executed"
                            );
                            let held = draft(
                                ctx,
                                &step_id,
                                EventType::TOOL_FAILED,
                                serde_json::json!({
                                    "tool_name": call.tool_name,
                                    "provider_tool_call_id": call.tool_call_id,
                                    "status": ToolStatus::Failed,
                                    "content": content,
                                    "outcome": APPROVAL_PENDING_OUTCOME,
                                    "approval_id": approval_id,
                                }),
                                Actor::Tool,
                            );
                            let held_event_id = held.event_id.as_str().to_string();
                            events.push(held);
                            // The turn ends here, but the call does not become
                            // a root: the chain records that it was held before
                            // it ran. The result, if a person grants it, happens
                            // between turns and belongs to the next turn's
                            // situation (#78) — this node claims no result.
                            let observation = turn.reason.node(
                                ReasonKind::Observation,
                                step_index,
                                format!(
                                    "{} held for approval: {approval_id}; not executed",
                                    call.tool_name
                                ),
                                vec![Basis::node(action_id.clone()), Basis::event(held_event_id)],
                                "observation/awaiting-approval",
                                ReasonSource::Kernel,
                            );
                            turn.reason_last_observation = Some(observation.id.clone());
                            events.push(reason_event(ctx, Some(&step_id), &observation));
                            turn.messages.push(ModelMessage::tool_for_call(
                                &step_id,
                                call.tool_call_id.clone(),
                                call.tool_name.clone(),
                                content,
                                true,
                            ));
                            return Ok(StepOutcome {
                                step_id,
                                events,
                                stop: Some(StopReason::NeedsUserInput),
                                final_content: Some(format!("approval pending: {approval_id}")),
                                spent: turn.spent,
                            });
                        }
                        // The text the model is given has to be the text the
                        // log keeps: a gateway failure that lives only in
                        // memory could never be rebuilt into this message.
                        let content = e.to_string();
                        let mut payload = serde_json::json!({
                            "tool_name": call.tool_name,
                            "provider_tool_call_id": call.tool_call_id,
                            "status": ToolStatus::Failed,
                            "content": content,
                            // Read back by the Scheduler when a resumed
                            // turn opens: a refusal is not a failure the
                            // gate should hold the turn on.
                            "outcome": GATEWAY_REFUSED_OUTCOME,
                        });
                        // Why it was refused, as a word rather than a sentence
                        // to parse: policy, a bad argument, an unknown name and
                        // an unenforceable declaration all land on this one
                        // channel, and adding them up by hand is what a log
                        // full of prose is good for only once. Omitted for the
                        // two answers that are not classes of refusal.
                        if let Some(kind) = e.kind() {
                            payload["error_kind"] = serde_json::json!(kind.as_str());
                        }
                        // `error_kind` says a permission rule refused it. This
                        // says which one — managed policy, a rule, or a
                        // capability the sandbox does not provide — which is
                        // the fact that tells a reader where to look.
                        if let ToolGatewayError::PolicyDenied { policy, .. } = &e {
                            payload["policy"] = serde_json::to_value(policy)?;
                        }
                        let refused =
                            draft(ctx, &step_id, EventType::TOOL_FAILED, payload, Actor::Tool);
                        let refused_event_id = refused.event_id.as_str().to_string();
                        events.push(refused);
                        // A gateway refusal (policy, resource, unknown tool)
                        // is a decision, not a result from the world — but it
                        // is the outcome of this call, and an action with no
                        // outcome is a root `why` cannot explain. The rule
                        // says which kind of outcome it is; the source is the
                        // kernel because the gateway, not the tool, answered.
                        let observation = turn.reason.node(
                            ReasonKind::Observation,
                            step_index,
                            format!("{} refused: {}", call.tool_name, first_line(&content)),
                            vec![Basis::node(action_id.clone()), Basis::event(refused_event_id)],
                            "observation/refused",
                            ReasonSource::Kernel,
                        );
                        turn.reason_last_observation = Some(observation.id.clone());
                        events.push(reason_event(ctx, Some(&step_id), &observation));
                        turn.messages.push(ModelMessage::tool_for_call(
                            &step_id,
                            call.tool_call_id.clone(),
                            call.tool_name.clone(),
                            content,
                            true,
                        ));
                        // A sandbox refusal is a gap, not a decision about
                        // this call; the second one ends the turn.
                        if turn.note_capability_gap(&e, &call.tool_name) {
                            return Ok(capability::stop(
                                ctx,
                                &step_id,
                                turn,
                                &mut events,
                                &tool_calls[index + 1..],
                            ));
                        }
                    }
                }
            }

            if let Some((reason, index)) = budget_stop {
                refusals::refuse_undispatched(
                    ctx,
                    &step_id,
                    turn,
                    &mut events,
                    &tool_calls[index..],
                    &reason,
                    turn.budget.max_output_tokens,
                );
                events.push(draft(
                    ctx,
                    &step_id,
                    EventType::STEP_COMPLETED,
                    serde_json::json!({}),
                    Actor::Agent,
                ));
                return Ok(StepOutcome {
                    step_id,
                    events,
                    stop: Some(reason),
                    final_content: None,
                    spent: turn.spent,
                });
            }

            events.push(draft(
                ctx,
                &step_id,
                EventType::STEP_COMPLETED,
                serde_json::json!({}),
                Actor::Agent,
            ));
        }

        // Whether this step learned anything, before the turn is handed back.
        turn.record_evidence();

        // The step is done and the turn could continue. Whether it does is the
        // caller's decision: the step budget is the Scheduler's policy, not the
        // loop's, and a loop that stopped itself could not be steered past the
        // step it just finished.
        Ok(StepOutcome { step_id, events, stop: None, final_content: None, spent: turn.spent })
    }
}

/// A copy of a model response whose tool proposals carry redacted arguments.
///
/// This is the copy that goes into the durable `response.completed` record, so
/// the long-lived log never holds a secret the model copied into a call. The
/// live turn keeps the raw response: the model is shown what it actually asked
/// for, and only the record is scrubbed (§7 row 4). The response shape is
/// preserved — a resumed call reads its arguments back out of the log.
pub(super) fn redacted_proposal(response: &ModelResponse, redactor: &Redactor) -> ModelResponse {
    let mut durable = response.clone();
    for block in &mut durable.content_blocks {
        if let ContentBlock::ToolUse { arguments, .. } = block {
            *arguments = redactor.redact_value(arguments).0;
        }
    }
    durable
}

/// A result's offloaded artifacts as locators a reader can follow, rather than
/// as content the model has to hold.
fn artifact_refs(result: &ToolResult) -> Vec<serde_json::Value> {
    result.artifacts.iter().map(ArtifactRef::reference).collect()
}

/// The capabilities a request depends on: streaming always, plus native tool
/// calling whenever tools are on offer.
fn required_capabilities(manifest: &ToolManifest) -> Vec<ModelCapability> {
    let mut capabilities = vec![ModelCapability::new(ModelCapability::STREAMING)];
    if !manifest.is_empty() {
        capabilities.push(ModelCapability::new(ModelCapability::TOOL_CALL));
    }
    capabilities
}

pub(super) fn step_failed(ctx: &TurnContext, step_id: &StepId) -> EventDraft {
    draft(ctx, step_id, EventType::STEP_FAILED, serde_json::json!({}), Actor::Agent)
}

/// Answer a held step's calls without running them.
///
/// A step-level gate refuses the whole batch, but a provider response's tool
/// calls each need a tool result in the history that follows: a dropped call
/// id makes the next request invalid (an OpenAI-compatible endpoint answers
/// HTTP 400), and a chat template rendered locally would be just as wrong. The
/// held calls did not run, so each result says so — `contents[i]` is the gate's
/// own text for call `i` — and carries `gate_refused`, which no reader of the
/// log mistakes for an observation about the world.
fn hold_step(
    ctx: &TurnContext,
    step_id: &StepId,
    turn: &mut LoopTurn,
    events: &mut Vec<EventDraft>,
    calls: &[ToolCall],
    contents: &[String],
) {
    for (call, content) in calls.iter().zip(contents) {
        events.push(draft(
            ctx,
            step_id,
            EventType::TOOL_FAILED,
            serde_json::json!({
                "tool_name": call.tool_name,
                "provider_tool_call_id": call.tool_call_id,
                "status": ToolStatus::Failed,
                "content": content,
                "outcome": GATE_REFUSED_OUTCOME,
            }),
            Actor::Tool,
        ));
        turn.messages.push(ModelMessage::tool_for_call(
            step_id,
            call.tool_call_id.clone(),
            call.tool_name.clone(),
            content.clone(),
            true,
        ));
    }
}
