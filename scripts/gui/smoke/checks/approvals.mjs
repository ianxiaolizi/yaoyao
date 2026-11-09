import { join } from 'node:path';
import { openPage, text, waitText } from '../browser.mjs';

export async function run(ctx) {
  const results = [];
  const check = (name, ok, detail = '') => {
    results.push({ name, ok, detail });
    ctx.check(name, ok, detail);
  };

  const { page, errors } = await openPage(
    ctx.context,
    ctx.origin,
    `?session=ui-approval#t=${ctx.token}`,
  );
  // Approval gate: four decisions, a TTL, and a settled record.
  await page.goto(`${ctx.origin}/?session=ui-approval#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#gate [data-decision="once"]', { timeout: 15_000 });
  check('审批卡四个决定', (await page.$$eval('#gate [data-decision]', (nodes) => nodes.length)) === 4);
  check('审批卡有 TTL 文案', /失效/.test(await text(page, '#gate .ttl')));
  const ttlFirst = await text(page, '#gate .ttl');
  await page.waitForTimeout(1_300);
  const ttlSecond = await text(page, '#gate .ttl');
  check('审批 TTL 逐秒走', ttlFirst !== ttlSecond && /后失效/.test(ttlSecond), `${ttlFirst} -> ${ttlSecond}`);
  await page.screenshot({ path: join(ctx.shots, 'm7-approval.png') });
  await page.click('#gate [data-decision="once"]');
  // The seeded approval has no live run behind it. What the page must do is
  // record the decision once, stop offering it, and take the card down — a
  // second click on a settled approval would be the interface lying.
  check('允许后记录为已允许', await waitText(page, '#transcript', '已允许', 20_000));
  check(
    '允许后不再可重复点',
    await page.$$eval('#gate [data-decision]', (nodes) => nodes.every((node) => node.disabled)),
  );
  check(
    '决定后审批卡撤下',
    await page
      .waitForFunction(() => document.querySelector('#gate')?.hidden === true, null, { timeout: 10_000 })
      .then(() => true)
      .catch(() => false),
  );

  await page.goto(`${ctx.origin}/?session=ui-expired#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#gate [data-resume]', { timeout: 15_000 });
  check('过期审批显示已过期', /已过期/.test(await text(page, '#gate')));
  check('过期审批可继续这一轮', Boolean(await page.$('#gate [data-resume]')));
  await page.screenshot({ path: join(ctx.shots, 'm7-expired.png') });

  // A new turn against the echo provider, then reconnect after one dropped SSE.
  await page.goto(`${ctx.origin}/?session=work#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  await page.click('#newTask');
  await page.fill('#input', 'hello smoke');
  await page.click('#send');
  check('新任务收到回答', await waitText(page, '.y-answer', 'echo: hello smoke', 25_000));
  check('新任务标题随第一轮更新', await waitText(page, '#title', 'hello smoke', 15_000));
  await page.screenshot({ path: join(ctx.shots, 'm7-sent.png') });

  const reconnect = await ctx.context.newPage();
  const streamPattern = '**/api/sessions/work/events?**';
  let dropped = 0;
  const handler = async (route) => {
    dropped += 1;
    await route.abort('failed');
    await reconnect.unroute(streamPattern, handler);
  };
  await reconnect.route(streamPattern, handler);
  await reconnect.goto(`${ctx.origin}/?session=work#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await reconnect.waitForSelector('.y-turn', { timeout: 15_000 });
  await reconnect.fill('#input', 'reconnect check');
  await reconnect.click('#send');
  check('断流一次后仍能收到回答', await waitText(reconnect, '.y-answer', 'echo: reconnect check', 25_000));
  await reconnect.waitForFunction(() => !document.querySelector('.y-live'), null, { timeout: 15_000 });
  const duplicates = await reconnect.$$eval('.y-answer', (nodes) =>
    nodes.filter((node) => (node.textContent ?? '').includes('echo: reconnect check')).length,
  );
  check('重连没有重复 item', dropped === 1 && duplicates === 1, `dropped=${dropped} copies=${duplicates}`);
  await reconnect.close();

  // 1000 items: load, unfold every past turn, and scroll.
  const long = await ctx.context.newPage();
  const longStart = Date.now();
  await long.goto(`${ctx.origin}/?session=perf-1000#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await long.waitForSelector('.y-turn', { timeout: 25_000 });
  const loadMs = Date.now() - longStart;
  check('1000 条 item 首次渲染 < 5s', loadMs < 5_000, `${loadMs}ms`);
  check('长会话保留最近两轮', (await long.$$eval('.y-past-row', (nodes) => nodes.length)) === 2);
  const expandMs = await long.evaluate(async () => {
    const started = performance.now();
    document.querySelector('[data-past-toggle]').click();
    await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    return performance.now() - started;
  });
  check('展开 97 个旧轮 < 2s', expandMs < 2_000, `${expandMs.toFixed(0)}ms`);
  const scrollMs = await long.evaluate(() => {
    const scroller = document.querySelector('#scroll');
    const started = performance.now();
    scroller.scrollTop = scroller.scrollHeight;
    void scroller.offsetHeight;
    return performance.now() - started;
  });
  check('滚到底部 < 500ms', scrollMs < 500, `${scrollMs.toFixed(0)}ms`);
  check('长会话仍是单列', (await long.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1)));
  await long.screenshot({ path: join(ctx.shots, 'm7-long.png') });
  await long.close();


  ctx.perf = { loadMs, expandMs: Math.round(expandMs), scrollMs: Math.round(scrollMs) };
  ctx.errors.push(...errors);
  await page.close();
  return results;
}
