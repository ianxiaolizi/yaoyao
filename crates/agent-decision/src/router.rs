//! The deterministic router: a task-class table with a default route.

use std::collections::BTreeMap;

use agent_ports::model_router::{ModelRoute, ModelRouter, RouteRequest};

/// Routes by task class from a fixed table.
///
/// A `BTreeMap` rather than a `HashMap` because the table is configuration: two
/// runs of the same profile must produce the same route, and nothing here should
/// depend on a hash seed.
pub struct TableModelRouter {
    routes: BTreeMap<String, ModelRoute>,
    /// The route for any task class the table does not name. It always exists,
    /// which is what makes `route` total.
    default: ModelRoute,
}

impl TableModelRouter {
    /// A router whose only route is `default`.
    ///
    /// This is the honest "no routing" configuration: one model, for everything.
    /// A caller that wants a router but has one model should say so with this,
    /// rather than with an `Option` that pushes the decision into the loop.
    pub fn new(default: ModelRoute) -> Self {
        Self { routes: BTreeMap::new(), default }
    }

    /// Add a route for one task class.
    pub fn with_task(mut self, task: impl Into<String>, route: ModelRoute) -> Self {
        self.routes.insert(task.into(), route);
        self
    }

    /// The route registered for one task class, if any.
    pub fn route_for(&self, task: &str) -> Option<&ModelRoute> {
        self.routes.get(task)
    }
}

impl ModelRouter for TableModelRouter {
    fn route(&self, request: &RouteRequest) -> ModelRoute {
        self.routes.get(request.task.as_str()).cloned().unwrap_or_else(|| self.default.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ports::model_router::{ModelBudget, ModelCapability, ModelTier, TaskClass};
    use agent_protocol::ids::{EventSeq, SessionId, TurnId};

    fn request(task: &str) -> RouteRequest {
        RouteRequest {
            session_id: SessionId::from_string("s"),
            turn_id: TurnId::from_string("t"),
            task: TaskClass::new(task),
            context_revision: EventSeq::ZERO,
            message_count: 1,
            required_capabilities: vec![ModelCapability::new(ModelCapability::STREAMING)],
            budget: ModelBudget::default(),
        }
    }

    fn route(provider: &str, model: &str) -> ModelRoute {
        ModelRoute::new(ModelTier::Main, provider, model)
    }

    #[test]
    fn a_named_task_gets_its_own_route() {
        let router = TableModelRouter::new(route("openai-compat", "big"))
            .with_task(TaskClass::SUMMARIZE, route("openai-compat", "small"));

        assert_eq!(router.route(&request(TaskClass::SUMMARIZE)).model, "small");
    }

    #[test]
    fn an_unnamed_task_falls_back_to_the_default() {
        let router = TableModelRouter::new(route("openai-compat", "big"));

        assert_eq!(router.route(&request(TaskClass::INTERACTIVE)).model, "big");
        assert_eq!(router.route(&request("something-new")).model, "big");
    }

    #[test]
    fn the_table_is_queryable_without_routing_a_request() {
        let router = TableModelRouter::new(route("openai-compat", "big"))
            .with_task(TaskClass::REVIEW, route("openai-compat", "picker"));

        assert_eq!(router.route_for(TaskClass::REVIEW).map(|r| r.model.as_str()), Some("picker"));
        assert!(router.route_for("unknown").is_none());
    }
}
