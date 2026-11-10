import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { openPage, text, waitText } from '../browser.mjs';

export async function run(ctx) {
  const results = [];
  const check = (name, ok, detail = '') => {
    results.push({ name, ok, detail });
    ctx.check(name, ok, detail);
  };

  const realSubagent = readFileSync(
    join(import.meta.dirname, '..', '..', 'fixtures', 'real-subagent', 'events.jsonl'),
    'utf8',
  );
  check('真实子代理样本：有委派', realSubagent.includes('subagent/spawned'));
  check('真实子代理样本：有结算', /subagent\/(completed|failed|cancelled)/.test(realSubagent));
  check('真实子代理样本：保留目标', realSubagent.includes('check the notes'));

  const { page, errors } = await openPage(ctx.context, ctx.origin, `?session=work#t=${ctx.token}`);
  // The page is the compiled bundle, not a dev server's response.
  const scripts = await page.$$eval('script[src]', (nodes) => nodes.map((node) => node.getAttribute('src')));
  check('页面来自打包产物 /assets/', scripts.some((src) => /^\.\/assets\/index-.*\.js$/.test(src)), scripts.join(','));

  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  check('会话标题取第一条用户消息', (await text(page, '#title')) === 'show the diff and think about it');
  check('轮数与日志一致', (await text(page, '#round')) === '第 4 轮');
  check('侧栏默认只列最近 6 条', (await page.$$eval('.y-session', (nodes) => nodes.length)) === 6);
  check('全部会话入口写明总数', (await text(page, '[data-session-all]')).includes('全部会话 · 11'));
  await page.click('[data-session-all]');
  await page.waitForFunction(() => document.querySelectorAll('.y-session').length === 11, null, {
    timeout: 10_000,
  });
  check('展开后列出全部会话', (await page.$$eval('.y-session', (nodes) => nodes.length)) === 11);
  check('当前轮目标', (await text(page, '.y-goal')).includes('what changed, and is it safe?'));
  check('当前轮回答', (await text(page, '.y-answer')).includes('one hunk'));
  check('思考折叠存在', (await page.$$eval('.y-thinking', (nodes) => nodes.length)) >= 1);

  await page.click('.y-action > summary');
  check('行动行展开后含参数', (await text(page, '.y-action')).includes('git.diff'));
  check('行动行展开后含 diff 输出', (await text(page, '.y-action')).includes('src/lib.rs'));

  check(
    '旧轮折叠，只留最近两轮',
    (await page.$$eval('.y-past-row', (nodes) => nodes.length)) === 2,
    String(await page.$$eval('.y-past-row', (nodes) => nodes.length)),
  );
  check('折叠入口写明更早轮数', (await text(page, '[data-past-toggle]')).includes('更早的 1 轮'));
  await page.click('[data-past-toggle]');
  check(
    '展开后所有旧轮都在',
    (await page.$$eval('.y-past-row', (nodes) => nodes.length)) === 3,
  );
  await page.screenshot({ path: join(ctx.shots, 'm7-work.png') });

  // The turn rail: hover/focus previews three lines, Alt↑↓ moves it.
  check('刻度按轮出现', (await page.$$eval('.y-tick', (nodes) => nodes.length)) === 4);
  await page.hover('.y-tick:nth-child(2)');
  await page.waitForSelector('.trace-pop.is-on', { timeout: 10_000 });
  const railText = await text(page, '.trace-pop');
  check('刻度 hover 摘要含轮次', railText.includes('第 2 轮'));
  check('刻度 hover 摘要含状态', /已回答|已验证完成|未完成|运行中|等待确认/.test(railText));
  check('刻度 hover 摘要含时间', /\d{2}:\d{2}/.test(railText));
  check('刻度 hover 摘要含步数', /步/.test(railText));
  const railBefore = await page.$eval('.trace-pop', (node) => node.getAttribute('data-turn'));
  await page.focus('.y-tick:nth-child(2)');
  await page.keyboard.press('Alt+ArrowDown');
  const railAfter = await page.$eval('.trace-pop', (node) => node.getAttribute('data-turn'));
  check('Alt↓ 预览下一轮', Boolean(railAfter) && railAfter !== railBefore);
  check(
    'Alt↓ 焦点落在刻度上',
    await page.evaluate(() => document.activeElement?.classList.contains('y-tick') ?? false),
  );
  await page.click('.y-tick:first-child');
  await page.waitForFunction(() => (document.querySelector('#scroll')?.scrollTop ?? 1) < 8, null, {
    timeout: 10_000,
  });
  check('点击刻度回到第一轮', true);

  // Evidence sheet: plan / diff / context / subagents.
  await page.click('#details');
  await page.waitForSelector('#sheet.open', { timeout: 10_000 });
  check('详情默认打开计划', await waitText(page, '#sheetBody', 'show the diff clearly'));
  await page.click('[data-panel-tab="diff"]');
  check('改动页签读到文件名', await waitText(page, '#sheetBody', 'src/lib.rs'));
  check('改动页签读到 patch', await waitText(page, '#sheetBody', '+new'));
  check('改动页签读到行数', await waitText(page, '#sheetBody', '−1'));
  await page.screenshot({ path: join(ctx.shots, 'm7-diff.png') });

  await page.click('[data-panel-tab="why"]');
  check('推理链页签读到判定', await waitText(page, '#sheetBody', 'answered from the diff'));
  check('推理链页签按轮标标题', (await text(page, '#sheetTitle')).includes('第 4 轮'));
  check('推理链节点带依据', await waitText(page, '#sheetBody', 'the turn context'));
  await page.screenshot({ path: join(ctx.shots, 'm8-why.png') });

  await page.click('[data-panel-tab="context"]');
  check('上下文页签读到用量', await waitText(page, '#sheetBody', '已用'));
  check('上下文页签读到分层', await waitText(page, '#sheetBody', 'state'));
  await page.click('[data-panel-tab="subagents"]');
  check('子代理页签读到目标', await waitText(page, '#sheetBody', 'summarize the diff'));
  check('子代理树读到第二层', await waitText(page, '#sheetBody', 'check the short path'));
  await page.screenshot({ path: join(ctx.shots, 'm7-sheet.png') });

  // Schedules: create and cancel against the real Inbox.
  await page.click('[data-panel-tab="schedules"]');
  await page.waitForSelector('#schedWhen', { timeout: 10_000 });
  await page.fill('#schedWhen', '+10m');
  await page.fill('#schedText', 'later smoke');
  await page.click('[data-schedule-add]');
  check('排定一条定时', await waitText(page, '#sheetBody', 'later smoke'));
  await page.click('[data-schedule-cancel]');
  check('取消定时后列表清空', await waitText(page, '#sheetBody', '没有待运行的定时'));
  await page.click('#sheetClose');
  check('侧板关闭', !(await page.$eval('#sheet', (node) => node.classList.contains('open'))));

  // The per-turn link opens the same chain for that turn.
  await page.click('.y-turn .y-why');
  check('轮末推理链链接打开该轮', await waitText(page, '#sheetBody', 'answered from the diff'));
  check('轮末链接沿用轮标题', (await text(page, '#sheetTitle')).includes('第 4 轮'));
  await page.click('#sheetClose');

  // Widths: drag, keyboard, double-click reset.
  const sideBefore = await page.$eval('#side', (node) => node.getBoundingClientRect().width);
  const sideBox = await (await page.$('#rzSide')).boundingBox();
  await page.mouse.move(sideBox.x + 3, sideBox.y + 200);
  await page.mouse.down();
  await page.mouse.move(sideBox.x + 43, sideBox.y + 200, { steps: 5 });
  await page.mouse.up();
  const sideDragged = await page.$eval('#side', (node) => node.getBoundingClientRect().width);
  check('拖动会话栏变宽', sideDragged > sideBefore + 20, `${Math.round(sideBefore)} -> ${Math.round(sideDragged)}`);
  await page.focus('#rzSide');
  await page.keyboard.press('ArrowRight');
  const sideKeyed = await page.$eval('#side', (node) => node.getBoundingClientRect().width);
  check('ArrowRight 也能调宽 8px', Math.round(sideKeyed) === Math.round(sideDragged) + 8, `${Math.round(sideDragged)} -> ${Math.round(sideKeyed)}`);
  await page.dblclick('#rzSide');
  const sideReset = await page.$eval('#side', (node) => node.getBoundingClientRect().width);
  check('双击复位会话栏宽度', Math.round(sideReset) === 236, String(Math.round(sideReset)));

  await page.click('#details');
  await page.waitForSelector('#sheet.open', { timeout: 10_000 });
  const sheetBefore = await page.$eval('#sheet', (node) => node.getBoundingClientRect().width);
  const sheetBox = await (await page.$('#rzSheet')).boundingBox();
  await page.mouse.move(sheetBox.x + 3, sheetBox.y + 200);
  await page.mouse.down();
  await page.mouse.move(sheetBox.x - 37, sheetBox.y + 200, { steps: 5 });
  await page.mouse.up();
  const sheetDragged = await page.$eval('#sheet', (node) => node.getBoundingClientRect().width);
  check('拖动详情面板变宽', sheetDragged > sheetBefore + 20, `${Math.round(sheetBefore)} -> ${Math.round(sheetDragged)}`);
  await page.dblclick('#rzSheet');
  const sheetReset = await page.$eval('#sheet', (node) => node.getBoundingClientRect().width);
  check('双击复位详情宽度', Math.round(sheetReset) === 420, String(Math.round(sheetReset)));
  await page.click('#sheetClose');

  // Export really downloads the rendered file.
  await page.click('#more');
  const [download] = await Promise.all([
    page.waitForEvent('download', { timeout: 15_000 }),
    page.click('[data-menu="export-md"]'),
  ]);
  const downloadPath = await download.path();
  const exported = downloadPath ? readFileSync(downloadPath, 'utf8') : '';
  check('导出 Markdown 文件名', download.suggestedFilename() === 'work.md', download.suggestedFilename());
  check('导出文件含会话内容', exported.includes('one hunk in src/lib.rs'));

  // Fork: a real child session, opened in place.
  await page.click('#more');
  await page.click('[data-menu="fork"]');
  check('分叉提示出现', await waitText(page, '#toast', '已从这一轮分叉出'));
  await page.waitForFunction(
    () => {
      const on = document.querySelector('.y-session.on');
      return Boolean(on) && on.dataset.session !== 'work';
    },
    null,
    { timeout: 20_000 },
  );
  check('分叉后打开子会话', (await page.$eval('.y-session.on', (node) => node.dataset.session)) !== 'work');
  check('子会话沿用标题', (await text(page, '#title')).includes('show the diff'));

  // Command palette.
  await page.keyboard.press('Control+K');
  await page.waitForSelector('#palette:not([hidden])', { timeout: 10_000 });
  await page.fill('#paletteInput', '计划');
  check('命令面板按输入过滤', (await page.$$eval('.y-cmd', (nodes) => nodes.length)) === 1);
  await page.keyboard.press('Enter');
  check('Enter 打开计划面板', await waitText(page, '#sheetBody', 'show the diff clearly'));
  await page.keyboard.press('Escape');
  check('Esc 关闭面板', !(await page.$eval('#sheet', (node) => node.classList.contains('open'))));

  // Incident regression: an answered turn with no completion evidence must not
  // be called "完成"; the kernel's unverified verdict and the local-backend
  // capability gap both have to reach the page.
  await page.goto(`${ctx.origin}/?session=unverified-delivery#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  check('事故回归：顶部状态是未验证交付', await waitText(page, '#statusPill', '已交付 · 未验证'));
  check('事故回归：顶部状态没有裸“完成”', !(await text(page, '#statusPill')).includes('完成'));
  check('事故回归：这一轮行末写已回答', await waitText(page, '.y-end', '已回答'));
  check('事故回归：未验证说明可见', await page.locator('#statusNote').isVisible());
  check(
    '事故回归：说明写明未验证不等于完成',
    await waitText(page, '#statusNote', '未验证的交付不等于完成'),
  );
  check('事故回归：local 长提示不再常驻', (await page.$('#envNote')) === null);
  check('事故回归：local 后端有按需入口', await page.locator('#envChip').isVisible());
  await page.click('#envChip');
  check('能力清单：local 菜单打开', await page.locator('#envMenu').isVisible());
  check('能力清单：写明当前后端', await waitText(page, '#envMenu', 'local'));
  check('能力清单：启动进程需要 AppContainer', await waitText(page, '#envMenu', '需要 AppContainer'));
  check('能力清单：进程自主联网不可用', await waitText(page, '#envMenu', '进程自主联网'));
  check(
    '能力清单：受限项有明确标记',
    (await text(page, '#envMenu')).includes('不可用'),
  );
  await page.screenshot({ path: join(ctx.shots, 'm10-capabilities.png') });
  await page.keyboard.press('Escape');
  check('能力清单：Esc 关闭', !(await page.locator('#envMenu').isVisible()));
  check(
    '事故回归：未验证提示可关闭',
    await page.locator('#statusNoteDismiss').isVisible(),
  );
  await page.hover('#details');
  await page.click('[data-evpanel="diff"]');
  await page.waitForSelector('#sheet.open', { timeout: 10_000 });
  check(
    '不完整改动清单有明确提示',
    await waitText(page, '#sheetBody', '改动清单不完整'),
  );
  await page.screenshot({ path: join(ctx.shots, 'm10-unverified.png') });
  await page.click('#sheetClose');
  await page.click('#statusNoteDismiss');
  check('事故回归：未验证提示关闭后隐藏', await page.$eval('#statusNote', (node) => node.hidden));
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  check('事故回归：刷新后同一提示仍保持关闭', await page.$eval('#statusNote', (node) => node.hidden));
  check('事故回归：顶部未验证状态仍在', await waitText(page, '#statusPill', '已交付 · 未验证'));

  // The positive shape: a passing check is the only way to say verified complete.
  await page.goto(`${ctx.origin}/?session=verified-delivery#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  check('已验证交付：顶部状态是已验证完成', await waitText(page, '#statusPill', '已验证完成'));
  check('已验证交付：没有未验证横幅', !(await page.locator('#statusNote').isVisible()));
  await page.screenshot({ path: join(ctx.shots, 'm10-verified.png') });

  // A new turn must not inherit the previous turn's verified status when the
  // status refresh fails after the terminal event.
  await page.evaluate(() => {
    const original = window.fetch.bind(window);
    window.__smokeOriginalFetch = original;
    window.__smokeStatusFailures = 0;
    window.fetch = (input, init) => {
      if (String(input).includes('/api/sessions/verified-delivery/status')) {
        window.__smokeStatusFailures += 1;
        return Promise.reject(new Error('forced status refresh failure'));
      }
      return original(input, init);
    };
  });
  await page.fill('#input', 'follow-up without status');
  await page.click('#send');
  check(
    '新回合仍能收到回答',
    await waitText(page, '.y-answer', 'echo: follow-up without status', 25_000),
  );
  await page.waitForFunction(
    () => document.querySelector('#stop')?.hidden && !document.querySelector('.y-live'),
    null,
    { timeout: 15_000 },
  );
  const statusRefreshes = await page.evaluate(() => window.__smokeStatusFailures);
  check('状态刷新请求已被拦截', statusRefreshes >= 1, `hits=${statusRefreshes}`);
  const statusAfterFailedRefresh = await text(page, '#statusPill');
  check(
    '状态刷新失败时不沿用上一轮已验证状态',
    !statusAfterFailedRefresh.includes('已验证完成'),
    statusAfterFailedRefresh,
  );
  await page.evaluate(() => {
    window.fetch = window.__smokeOriginalFetch;
  });

  // A capability stop delivered nothing: the page must not dress its ask as a
  // delivered result, and must offer the backend switch.
  await page.goto(`${ctx.origin}/?session=capability-blocked#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  check('能力停止：顶部状态是未完成已阻塞', await waitText(page, '#statusPill', '未完成 · 已阻塞'));
  check('能力停止：顶部状态没有写成已交付', !(await text(page, '#statusPill')).includes('已交付'));
  check('能力停止：说明给出 app-container', await waitText(page, '#statusNote', 'app-container'));
  // The ask is rendered as Markdown, so the ordered list has no literal "1."
  // in its text; the option wording is what proves all three stayed.
  check('能力停止：转写保留三分支', (await text(page, '.y-answer')).includes('接受降级交付'));
  check('能力停止：转写保留取消项', (await text(page, '.y-answer')).includes('取消这个任务'));
  await page.screenshot({ path: join(ctx.shots, 'm10-capability-blocked.png') });

  // The real chain: captured from an actual turn (scheduler, gateway, fs,
  // git diff), so the panels are checked against real event shapes too.
  await page.goto(`${ctx.origin}/?session=real-chain#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  check('真实执行链：标题取真实输入', (await text(page, '#title')).includes('修一下'));
  check('真实执行链：行动行有 fs.read', await waitText(page, '#transcript', 'fs.read'));
  check('真实执行链：行动行有 git.diff', await waitText(page, '#transcript', 'git.diff'));
  check('真实执行链：回答是模型原话', await waitText(page, '.y-answer', '改好了'));
  check('真实执行链：回答保留最终结论', await waitText(page, '.y-answer', 'end 现在是闭区间'));
  await page.click('#details');
  check(
    '真实执行链：没有真计划时明确降级',
    await waitText(page, '#sheetBody', '这个会话还没有 plan.update'),
  );
  await page.click('[data-panel-tab="diff"]');
  check('真实执行链：改动面板读到真实文件', await waitText(page, '#sheetBody', 'src/parser.mjs'));
  check('真实执行链：patch 是 git 的输出', await waitText(page, '#sheetBody', 'diff --git'));
  const realDiffText = await text(page, '#sheetBody');
  check('真实执行链：diff 摘要来自工具结果', realDiffText.includes('+11') && realDiffText.includes('−3'));
  await page.click('[data-panel-tab="why"]');
  await page.waitForSelector('.y-why-verdict', { timeout: 10_000 }).catch(() => null);
  check('真实执行链：推理链节点成链', (await page.$$eval('.y-node', (nodes) => nodes.length)) >= 10);
  check('真实执行链：判定来自 verdict 节点', await waitText(page, '#sheetBody', 'verdict/answer'));
  check('真实执行链：链自检通过', !(await text(page, '#sheetBody')).includes('自检没有全过'));
  await page.screenshot({ path: join(ctx.shots, 'm9-real-chain.png') });
  await page.click('#sheetClose');

  // Model switch: the menu comes from /api/meta, the choice is written to the
  // session's log, and the page must not promise it applies to a running turn.
  await page.click('#modelChip');
  await page.waitForSelector('#modelMenu:not([hidden])', { timeout: 10_000 });
  const modelOptions = await page.$$eval('#modelMenu [data-provider]', (nodes) =>
    nodes.map((node) => (node.textContent ?? '').trim()),
  );
  check('模型菜单列出本进程可用的模型', modelOptions.some((label) => label.includes('echo')), modelOptions.join(','));
  await page.click('#modelMenu [data-provider="echo"]');
  check('切换模型给出下一轮生效提示', await waitText(page, '#toast', '下一轮生效', 15_000));
  await page.click('#modelChip');
  await page.screenshot({ path: join(ctx.shots, 'm8-model.png') });
  check(
    '菜单把当前模型标成已选',
    await page
      .waitForFunction(() => document.querySelector('#modelMenu [data-provider="echo"]')?.textContent?.startsWith('✓'))
      .then(() => true)
      .catch(() => false),
  );
  await page.keyboard.press('Escape');

  // Approval policy: a statement, not an auto-approve switch.
  await page.click('#policyChip');
  await page.waitForSelector('#policyMenu:not([hidden])', { timeout: 10_000 });
  check('审批策略菜单写明逐次确认', await waitText(page, '#policyMenu', '逐次确认'));
  check('审批策略菜单拒绝自动批准', await waitText(page, '#policyMenu', '本版不提供'));
  check(
    '审批策略项不可点',
    (await page.$$eval('#policyMenu .pop-item', (nodes) => nodes.every((node) => node.disabled))),
  );
  check('附件按钮明确禁用', await page.$eval('#attach', (node) => node.disabled));
  await page.screenshot({ path: join(ctx.shots, 'm10-policy.png') });
  await page.keyboard.press('Escape');

  // Archive, restore, delete — last, because deleting moves the page to another
  // session. The fixture is its own session so the rest of the run is unaffected.
  await page.goto(`${ctx.origin}/?session=scratch#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  await page.click('#more');
  await page.click('[data-menu="archive"]');
  check('归档提示', await waitText(page, '#toast', '已归档', 15_000));
  check(
    '归档会话进已归档区',
    await page
      .waitForFunction(() => [...document.querySelectorAll('.y-session.arch')].some((node) => node.dataset.session === 'scratch'))
      .then(() => true)
      .catch(() => false),
  );
  check('侧栏出现已归档分区', (await text(page, '#sessionList')).includes('已归档'));
  await page.screenshot({ path: join(ctx.shots, 'm8-archive.png') });
  await page.waitForFunction(
    () => document.querySelector('[data-menu="archive"]')?.textContent?.trim() === '恢复此会话',
    null,
    { timeout: 10_000 },
  );
  await page.click('#more');
  await page.click('[data-menu="archive"]');
  check('恢复提示', await waitText(page, '#toast', '已恢复', 15_000));
  check(
    '恢复后离开已归档区',
    await page
      .waitForFunction(() => ![...document.querySelectorAll('.y-session.arch')].some((node) => node.dataset.session === 'scratch'))
      .then(() => true)
      .catch(() => false),
  );

  await page.click('#more');
  await page.click('[data-menu="remove"]');
  check('删除先要确认', (await text(page, '#menu [data-menu="remove"]')).trim() === '再点一次删除');
  await page.click('#menu [data-menu="remove"]');
  check('删除提示', await waitText(page, '#toast', '已删除这个会话', 20_000));
  check(
    '删除后侧栏不再有它',
    await page
      .waitForFunction(() => !document.querySelector('[data-session="scratch"]'), null, { timeout: 10_000 })
      .then(() => true)
      .catch(() => false),
  );


  ctx.errors.push(...errors);
  await page.close();
  return results;
}
