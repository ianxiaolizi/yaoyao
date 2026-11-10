import { join } from 'node:path';
import { openPage } from '../browser.mjs';

export async function run(ctx) {
  const results = [];
  const check = (name, ok, detail = '') => {
    results.push({ name, ok, detail });
    ctx.check(name, ok, detail);
  };

  const { page, errors } = await openPage(ctx.context, ctx.origin, `?session=work#t=${ctx.token}`);
  // Reduced motion: the two moving surfaces arrive without travelling.
  await page.emulateMedia({ reducedMotion: 'reduce' });
  await page.goto(`${ctx.origin}/?session=work#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('.y-turn', { timeout: 15_000 });
  const motion = await page.evaluate(() => {
    const sheet = getComputedStyle(document.querySelector('#sheet'));
    const result = getComputedStyle(document.querySelector('#result'));
    return {
      sheet: sheet.transitionDuration,
      sheetAnimation: sheet.animationName,
      result: result.transitionDuration,
    };
  });
  check('减少动效：侧板无过渡', motion.sheet === '0s', motion.sheet);
  check('减少动效：成果台无过渡', motion.result === '0s', motion.result);
  await page.emulateMedia({ reducedMotion: 'no-preference' });

  // 1024: below the 1080 drawer breakpoint, above the 720 compact one; the
  // plan's "1440 → 1024 不塌" is measured here rather than only at 900/390.
  const laptop = await ctx.context.newPage();
  await laptop.setViewportSize({ width: 1024, height: 900 });
  await laptop.goto(`${ctx.origin}/?session=work#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await laptop.waitForSelector('.y-turn', { timeout: 15_000 });
  check('1024 无横向溢出', await laptop.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1));
  check('1024 侧栏变抽屉', await laptop.$eval('.y-app', (node) => node.classList.contains('y-side-off')));
  await laptop.screenshot({ path: join(ctx.shots, 'm10-1024.png') });
  await laptop.close();

  // Narrow screens.
  const narrow = await ctx.context.newPage();
  await narrow.setViewportSize({ width: 900, height: 900 });
  await narrow.goto(`${ctx.origin}/?session=work#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await narrow.waitForSelector('.y-turn', { timeout: 15_000 });
  check('900px 默认收起侧栏', await narrow.$eval('.y-app', (node) => node.classList.contains('y-side-off')));
  await narrow.click('#toggleSide');
  check('☰ 打开抽屉', await narrow.$eval('.y-app', (node) => node.classList.contains('y-side-open')));
  check('抽屉有遮罩', await narrow.isVisible('#sideMask'));
  await narrow.screenshot({ path: join(ctx.shots, 'm7-narrow.png') });
  await narrow.click('#sideMask');
  check('点遮罩关抽屉', await narrow.$eval('.y-app', (node) => node.classList.contains('y-side-off')));
  await narrow.close();

  const phone = await ctx.context.newPage();
  await phone.setViewportSize({ width: 390, height: 844 });
  await phone.goto(`${ctx.origin}/?session=work#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await phone.waitForSelector('.y-turn', { timeout: 15_000 });
  check('390 无横向溢出', await phone.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1));
  check('390 输入框仍在', await phone.isVisible('#input'));
  await phone.click('#details');
  check('390 点详情打开侧板（触屏等价）', await phone.$eval('#sheet', (node) => node.classList.contains('open')));
  await phone.screenshot({ path: join(ctx.shots, 'm7-390.png') });
  await phone.close();


  ctx.errors.push(...errors);
  await page.close();
  return results;
}
