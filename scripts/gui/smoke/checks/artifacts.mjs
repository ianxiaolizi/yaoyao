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
    `?session=artifact-smoke#t=${ctx.token}`,
  );
  // Artifacts: image, HTML, Markdown and PDF previews from the embedded page.
  await page.goto(`${ctx.origin}/?session=artifact-smoke#t=${ctx.token}`, { waitUntil: 'domcontentloaded' });
  await page.waitForSelector('#result:not([hidden])', { timeout: 15_000 });
  check('成果条数', (await text(page, '#resultCount')).includes('8 个产出'));
  await page.click('#resultMore');
  await page.waitForSelector('.y-result.open', { timeout: 10_000 });
  await page.waitForSelector('#artifactPreview img', { timeout: 10_000 });
  check(
    '图片 artifact 预览',
    await page.$eval('#artifactPreview img', (node) => node.complete && node.naturalWidth > 0),
  );
  await page.screenshot({ path: join(ctx.shots, 'm7-artifacts.png') });

  await page.click('#artifactList button:has-text("junction-fix.md")');
  check('Markdown artifact 渲染', await waitText(page, '.y-art-md', 'junction 修复验证'));
  check(
    'Markdown 表格',
    (await page.$$eval('.y-art-md table th', (nodes) => nodes.map((node) => node.textContent?.trim()))).join('|') ===
      '项|结果',
  );
  check('Markdown 表格体', (await text(page, '.y-art-md')).includes('repro_junction.mjs'));
  check('Markdown 有序列表', (await page.$$eval('.y-art-md ol li', (nodes) => nodes.length)) === 3);
  check(
    'Markdown 链接',
    await page.$eval('.y-art-md a', (node) => node.getAttribute('href') === 'https://example.com/report' && node.getAttribute('target') === '_blank'),
  );
  await page.waitForSelector('.y-art-md img[alt="内嵌图"]', { timeout: 10_000 });
  check(
    'Markdown 内嵌图',
    await page.$eval('.y-art-md img[alt="内嵌图"]', (node) => node.complete && node.naturalWidth > 0),
  );
  check('Markdown 外链图不加载', (await page.$$('.y-art-md img[alt="外链图"]')).length === 0);
  check('Markdown 外链图降级为链接', await waitText(page, '.y-art-md', '图片：外链图'));
  await page.click('#artifactList button:has-text("report.html")');
  const frame = await page.waitForSelector('.y-art-frame', { timeout: 10_000 }).catch(() => null);
  check(
    'HTML artifact 走无脚本 iframe',
    Boolean(frame) && ((await frame.getAttribute('srcdoc')) ?? '').includes('36 / 36'),
  );
  await page.click('#artifactList button:has-text("coverage.pdf")');
  const pdf = await page.waitForSelector('.y-art-pdf', { timeout: 10_000 }).catch(() => null);
  check(
    'PDF artifact 走内嵌查看器',
    Boolean(pdf) &&
      (await pdf.getAttribute('type')) === 'application/pdf' &&
      ((await pdf.getAttribute('src')) ?? '').startsWith('blob:'),
  );
  await page.click('#artifactList button:has-text("notes.docx")');
  check('docx 预览读到第一段', await waitText(page, '.y-art-docx', 'M9 文档预览'));
  check('docx 预览读到第二段', await waitText(page, '.y-art-docx', '页面解压'));
  check('docx 转义符还原', await waitText(page, '.y-art-docx', '带 & 一个转义符'));
  check(
    'docx 标题样式保留',
    (await text(page, '.y-art-docx h3')) === 'M9 文档预览',
    await text(page, '.y-art-docx h3'),
  );
  const docxImage = await page.evaluate(() => {
    const image = document.querySelector('.y-art-docx img');
    return image ? { src: image.src, alt: image.alt } : null;
  });
  check('docx 嵌入图片走 data URL', Boolean(docxImage?.src.startsWith('data:image/png;base64,')), JSON.stringify(docxImage));
  check('docx 图片保留 alt', docxImage?.alt === '内嵌 PNG', JSON.stringify(docxImage));
  const docxRun = await page.evaluate(() => {
    const node = [...document.querySelectorAll('.y-art-docx .y-run')].find((row) => row.textContent?.includes('加粗斜体'));
    if (!node) return null;
    const style = getComputedStyle(node);
    return { text: node.textContent, weight: style.fontWeight, fontStyle: style.fontStyle, size: style.fontSize, color: style.color };
  });
  check('docx run 粗体', Number(docxRun?.weight) >= 600, JSON.stringify(docxRun));
  check('docx run 斜体', docxRun?.fontStyle === 'italic', JSON.stringify(docxRun));
  check('docx run 字号', parseFloat(docxRun?.size ?? '0') > 20, JSON.stringify(docxRun));
  check('docx run 前景色', docxRun?.color === 'rgb(192, 57, 43)', JSON.stringify(docxRun));
  await page.screenshot({ path: join(ctx.shots, 'm11-office.png') });
  await page.click('#artifactList button:has-text("broken.docx")');
  check('docx 缺关系文件仍显示正文', await waitText(page, '.y-art-docx', '降级文档正文仍可读'));
  check('docx 缺图显示占位', await waitText(page, '.y-art-docx', '缺图占位'));
  check('docx 缺图给出警告', await waitText(page, '.y-art-warning', '缺少图片关系文件'));
  await page.click('#artifactList button:has-text("numbers.xlsx")');
  check('xlsx 预览读到单元格', await waitText(page, '.y-art-sheet', '42'));
  check('xlsx 预览带工作表名', await waitText(page, '.y-art-sheet-name', 'sheet1.xml'));
  check('xlsx 日期格式', await waitText(page, '.y-art-sheet', '2024-01-01'));
  check('xlsx 合并单元格', await waitText(page, '.y-art-sheet td[colspan="2"]', '合计'));
  await page.click('#artifactList button:has-text("broken.xlsx")');
  check('xlsx 缺样式仍显示主体', await waitText(page, '.y-art-sheet', '一月'));
  check('xlsx 缺样式降级为原值', await waitText(page, '.y-art-sheet', '45292'));
  check('xlsx 缺样式给出警告', await waitText(page, '.y-art-warning', '缺少 xl/styles.xml'));

  await page.click('#resultFull');
  await page.waitForSelector('.y-result.full', { timeout: 10_000 });
  check('成果台全屏', await page.$eval('#resultFull', (node) => node.getAttribute('aria-pressed') === 'true'));
  // Evidence in the full-screen state: the preview is unambiguously the thing
  // on screen, with no absolute-positioned siblings to clip it.
  await page.waitForTimeout(300);
  await page.screenshot({ path: join(ctx.shots, 'm9-docx-xlsx.png') });
  await page.keyboard.press('Escape');
  check('Esc 退出全屏但保留成果台', await page.$eval('#result', (node) => node.classList.contains('open') && !node.classList.contains('full')));
  await page.keyboard.press('Escape');
  check('再按 Esc 收起成果台', !(await page.$eval('#result', (node) => node.classList.contains('open'))));


  ctx.errors.push(...errors);
  await page.close();
  return results;
}
