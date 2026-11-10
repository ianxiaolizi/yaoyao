// GUI smoke fixtures: every session is written to the throwaway data dir.
import { createHash, randomUUID } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { deflateRawSync } from 'node:zlib';

export function seedAll(dataDir) {
const nowIso = () => new Date().toISOString();

function event(session, seq, type, { turn = null, step = null, actor = 'agent', payload = {} } = {}) {
  const at = nowIso();
  return {
    seq,
    recorded_at: at,
    event: {
      event_id: randomUUID(),
      event_type: type,
      schema_version: '0.1.0',
      session_id: session,
      run_id: null,
      turn_id: turn,
      step_id: step,
      actor,
      created_at: at,
      causation_id: null,
      correlation_id: null,
      payload,
      metadata: {},
      ignorable: false,
    },
  };
}

function inbox(session, itemId, text) {
  return {
    created_at: nowIso(),
    item_id: itemId,
    kind: 'next_turn',
    payload: { text },
    priority: 0,
    session_id: session,
    status: 'pending',
  };
}

function reason(turn, step, kind, claim, id) {
  return {
    node: {
      id,
      turn_id: turn,
      step,
      kind,
      claim,
      basis: [{ fact: 'the smoke fixture' }],
      rule: 'situation/derive',
      source: 'kernel',
    },
  };
}

function writeSession(session, events) {
  const dir = join(dataDir, session);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'events.jsonl'), events.map((row) => JSON.stringify(row)).join('\n') + '\n');
}

/** A small log builder: one sequence, one turn at a time. */
function builder(session) {
  const events = [];
  let seq = 0;
  const add = (type, { turn = null, step = null, actor = 'agent', payload = {} } = {}) => {
    events.push(event(session, seq++, type, { turn, step, actor, payload }));
  };
  return {
    add,
    events,
    ask(turn, text) {
      const item = inbox(session, `inbox-${turn}`, text);
      add('inbox/appended', { actor: 'user', payload: item });
      add('turn/started', { turn, actor: 'scheduler', payload: { inbox_item: item.item_id } });
      add('step/started', { turn, step: `step-${turn}` });
    },
    answer(turn, text, extra = {}) {
      add('turn/completed', { turn, actor: 'scheduler', payload: { final_content: text, ...extra } });
    },
    save() {
      writeSession(session, events);
    },
  };
}

const diff = {
  files: [
    {
      file: 'src/lib.rs',
      patch: '@@ -1,1 +1,2 @@\n-old\n+new\n+more',
      additions: 2,
      deletions: 1,
      status: 'modified',
    },
  ],
  file_count: 1,
  additions: 2,
  deletions: 1,
};

function seedWork() {
  const log = builder('work');
  log.ask('t1', 'show the diff and think about it');
  log.answer('t1', 'One hunk in src/lib.rs.');
  log.ask('t2', 'turn two');
  log.answer('t2', 'Two.');
  log.ask('t3', 'turn three');
  log.answer('t3', 'Three.');

  // The current turn carries everything the evidence sheet reads.
  log.ask('t4', 'what changed, and is it safe?');
  log.add('reason/recorded', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      node: {
        id: 'r/1',
        turn_id: 't4',
        step: 0,
        kind: 'situation',
        claim: 'the person asked what changed',
        basis: [{ fact: 'the turn context' }],
        rule: 'situation/derive',
        source: 'kernel',
      },
    },
  });
  log.add('reason/recorded', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      node: {
        id: 'r/2',
        turn_id: 't4',
        step: 0,
        kind: 'reading',
        claim: 'the input asks a question, so the turn answers it',
        basis: [{ node: 'r/1' }],
        rule: 'reading/question-cue',
        source: 'kernel',
      },
    },
  });
  log.add('reason/recorded', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      node: {
        id: 'r/3',
        turn_id: 't4',
        step: 0,
        kind: 'stance',
        claim: 'answering; no call is needed',
        basis: [{ node: 'r/2' }],
        rule: 'stance/answer-question',
        source: 'kernel',
      },
    },
  });
  log.add('reason/recorded', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      node: {
        id: 'r/4',
        turn_id: 't4',
        step: 0,
        kind: 'verdict',
        claim: 'answered from the diff',
        basis: [{ node: 'r/3' }],
        rule: 'verdict/answer',
        source: 'kernel',
      },
    },
  });
  log.add('tool/started', {
    turn: 't4',
    step: 'step-t4',
    payload: { tool_name: 'git.diff', provider_tool_call_id: 'c1', arguments: { staged: false } },
  });
  log.add('tool/completed', {
    turn: 't4',
    step: 'step-t4',
    actor: 'tool',
    payload: {
      tool_name: 'git.diff',
      provider_tool_call_id: 'c1',
      status: 'completed',
      content: JSON.stringify(diff),
    },
  });
  log.add('model/request.prepared', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      estimated_context_tokens: 100,
      estimated_messages_tokens: 150,
      estimated_tool_tokens: 20,
      estimated_request_tokens: 170,
      max_input_tokens: 8000,
      reserved_output_tokens: 2000,
      message_count: 10,
      context_message_count: 8,
      dropped_message_count: 2,
      tool_count: 7,
      tool_manifest_id: 'smoke-manifest',
      context_layers: {
        state: { message_uid: 'msg/state/t4', text: 'the state layer' },
        task: { message_uid: 'msg/task/t4', text: 'the task layer' },
      },
      provider_usage: {
        input_tokens: 170,
        output_tokens: 20,
        total_tokens: 190,
        cache_hit_tokens: 80,
        reasoning_tokens: 5,
      },
    },
  });
  log.add('subagent/spawned', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      child_session_id: 'work-child',
      provider: 'mock',
      parent_turn_id: 't4',
      fork_at_turn_id: 't4',
      request: { objective: 'summarize the diff' },
    },
  });
  log.add('subagent/completed', {
    turn: 't4',
    step: 'step-t4',
    actor: 'system',
    payload: {
      child_session_id: 'work-child',
      provider: 'mock',
      parent_turn_id: 't4',
      outcome: { status: 'completed', answer: 'done' },
    },
  });
  log.add('plan/updated', {
    turn: 't4',
    step: 'step-t4',
    payload: {
      provider_tool_call_id: 'p1',
      plan: {
        goal: 'show the diff clearly',
        items: [
          { text: 'read the diff', status: 'done' },
          { text: 'explain it', status: 'doing' },
        ],
        facts: ['the log is the source'],
      },
    },
  });
  log.answer('t4', 'The diff is one hunk in src/lib.rs.');
  log.save();

  // The child's own log exists and has one grandchild, so the tree can read two
  // levels without probing a session that is not there (a 404 the browser would
  // log as a console error).
  const child = builder('work-child');
  child.ask('c1', 'review the diff');
  child.add('subagent/spawned', {
    turn: 'c1',
    step: 'step-c1',
    payload: {
      child_session_id: 'work-grandchild',
      provider: 'mock',
      parent_turn_id: 'c1',
      fork_at_turn_id: 'c1',
      request: { objective: 'check the short path' },
    },
  });
  child.answer('c1', 'looks fine');
  child.save();
}

/** A minimal one-page PDF with a real xref, so the viewer has something to open. */
function pdfBytes() {
  const stream = 'BT /F1 12 Tf 20 100 Td (M8 smoke) Tj ET';
  const bodies = [
    '<< /Type /Catalog /Pages 2 0 R >>',
    '<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
    '<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>',
    `<< /Length ${stream.length} >>\nstream\n${stream}\nendstream`,
    '<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
  ];
  let out = '%PDF-1.4\n';
  const offsets = [];
  for (const [index, body] of bodies.entries()) {
    offsets.push(out.length);
    out += `${index + 1} 0 obj\n${body}\nendobj\n`;
  }
  const xref = out.length;
  out += `xref\n0 ${bodies.length + 1}\n0000000000 65535 f \n`;
  out += offsets.map((offset) => `${String(offset).padStart(10, '0')} 00000 n \n`).join('');
  out += `trailer\n<< /Size ${bodies.length + 1} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return Buffer.from(out, 'latin1');
}

function putArtifact(bytes) {
  const hash = createHash('sha256').update(bytes).digest('hex');
  const dir = join(dataDir, 'artifacts');
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, `${hash}.blob`), bytes);
  return { locator: `artifact://sha256/${hash}`, hash };
}

/** A ZIP with deflated entries, the way a real OOXML file is packed. */
function zipBytes(entries) {
  const local = [];
  const central = [];
  let offset = 0;
  for (const [name, content] of entries) {
    const data = Buffer.isBuffer(content) ? content : Buffer.from(content, 'utf8');
    const packed = deflateRawSync(data);
    const nameBytes = Buffer.from(name, 'utf8');
    const header = Buffer.alloc(30);
    header.writeUInt32LE(0x04034b50, 0);
    header.writeUInt16LE(20, 4);
    header.writeUInt16LE(0x0800, 6);
    header.writeUInt16LE(8, 8);
    header.writeUInt32LE(0, 14);
    header.writeUInt32LE(packed.length, 18);
    header.writeUInt32LE(data.length, 22);
    header.writeUInt16LE(nameBytes.length, 26);
    local.push(header, nameBytes, packed);
    const entry = Buffer.alloc(46);
    entry.writeUInt32LE(0x02014b50, 0);
    entry.writeUInt16LE(20, 4);
    entry.writeUInt16LE(20, 6);
    entry.writeUInt16LE(0x0800, 8);
    entry.writeUInt16LE(8, 10);
    entry.writeUInt32LE(packed.length, 20);
    entry.writeUInt32LE(data.length, 24);
    entry.writeUInt16LE(nameBytes.length, 28);
    entry.writeUInt32LE(offset, 42);
    central.push(entry, nameBytes);
    offset += 30 + nameBytes.length + packed.length;
  }
  const directory = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...local, directory, end]);
}

function docxBytes() {
  // A 1x1 PNG, kept as bytes so zipBytes proves embedded media survives intact.
  const pixel = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=',
    'base64',
  );
  return zipBytes([
    [
      '[Content_Types].xml',
      '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Default Extension="png" ContentType="image/png"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>',
    ],
    [
      '_rels/.rels',
      '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>',
    ],
    [
      'word/styles.xml',
      '<?xml version="1.0" encoding="UTF-8"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/></w:style><w:style w:type="paragraph" w:styleId="Body"><w:name w:val="Body Text"/></w:style></w:styles>',
    ],
    [
      'word/_rels/document.xml.rels',
      '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rIdImage" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/image" Target="media/pixel.png"/></Relationships>',
    ],
    ['word/media/pixel.png', pixel],
    [
      'word/document.xml',
      '<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:body><w:p><w:pStyle w:val="Heading1"/><w:r><w:t>M9 文档预览</w:t></w:r></w:p><w:p><w:r><w:rPr><w:b/><w:i/><w:sz w:val="32"/><w:color w:val="C0392B"/></w:rPr><w:t>加粗斜体红色大字</w:t></w:r><w:r><w:t> 普通正文</w:t></w:r></w:p><w:p><w:r><w:t>第二段：docx 由页面解压读取。</w:t></w:r></w:p><w:p><w:r><w:drawing><wp:inline><wp:docPr id="1" name="pixel" descr="内嵌 PNG"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><a:blip r:embed="rIdImage"/></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p><w:p><w:r><w:t>第三个段落带 &amp; 一个转义符。</w:t></w:r></w:p></w:body></w:document>',
    ],
  ]);
}

function xlsxBytes() {
  return zipBytes([
    [
      'xl/sharedStrings.xml',
      '<?xml version="1.0" encoding="UTF-8"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>月份</t></si><si><t>收入</t></si><si><t>一月</t></si><si><t>日期</t></si><si><t>合计</t></si></sst>',
    ],
    [
      'xl/styles.xml',
      '<?xml version="1.0" encoding="UTF-8"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><numFmts count="1"><numFmt numFmtId="164" formatCode="yyyy-mm-dd"/></numFmts><cellXfs count="2"><xf numFmtId="0"/><xf numFmtId="164"/></cellXfs></styleSheet>',
    ],
    [
      'xl/worksheets/sheet1.xml',
      '<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c><c r="C1" t="s"><v>3</v></c></row><row r="2"><c r="A2" t="s"><v>2</v></c><c r="B2"><v>42</v></c><c r="C2" s="1"><v>45292</v></c></row><row r="3"><c r="A3" t="s"><v>4</v></c><c r="C3"><v>100</v></c></row></sheetData><mergeCells count="1"><mergeCell ref="A3:B3"/></mergeCells></worksheet>',
    ],
  ]);
}

function brokenDocxBytes() {
  return zipBytes([
    [
      '[Content_Types].xml',
      '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>',
    ],
    [
      'word/document.xml',
      '<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><w:body><w:p><w:r><w:t>降级文档正文仍可读。</w:t></w:r></w:p><w:p><w:r><w:drawing><wp:inline><wp:docPr id="2" name="missing" descr="缺图占位"/><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/picture"><a:blip r:embed="rIdMissing"/></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p></w:body></w:document>',
    ],
  ]);
}

function brokenXlsxBytes() {
  return zipBytes([
    [
      'xl/sharedStrings.xml',
      '<?xml version="1.0" encoding="UTF-8"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>一月</t></si></sst>',
    ],
    [
      'xl/worksheets/sheet1.xml',
      '<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" s="1"><v>45292</v></c></row></sheetData></worksheet>',
    ],
  ]);
}

function seedArtifacts() {
  const log = builder('artifact-smoke');
  log.ask('t1', 'show the artifacts');
  const png = Buffer.from('R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==', 'base64');
  const html = Buffer.from(
    '<!doctype html><html><body style="font:14px system-ui;padding:24px"><h1>junction 修复验证</h1><p>36 / 36 通过</p></body></html>',
    'utf8',
  );
  const md = Buffer.from(
    [
      '# junction 修复验证',
      '',
      '| 项 | 结果 |',
      '| --- | --- |',
      '| 复现 | repro_junction.mjs |',
      '| 回归 | 36 / 36 |',
      '',
      '1. 复现',
      '2. 修复 lexical + realpath',
      '3. 回归',
      '',
      '[完整报告](https://example.com/report)',
      '',
      `![内嵌图](data:image/gif;base64,${png.toString('base64')})`,
      '',
      '![外链图](https://example.com/never-loaded.png)',
      '',
    ].join('\n'),
    'utf8',
  );
  for (const [label, bytes] of [
    ['shot.gif', png],
    ['report.html', html],
    ['junction-fix.md', md],
    ['coverage.pdf', pdfBytes()],
    ['notes.docx', docxBytes()],
    ['numbers.xlsx', xlsxBytes()],
    ['broken.docx', brokenDocxBytes()],
    ['broken.xlsx', brokenXlsxBytes()],
  ]) {
    const { locator } = putArtifact(bytes);
    const call = `art-${label}`;
    log.add('tool/started', {
      turn: 't1',
      step: 'step-t1',
      payload: { tool_name: 'fs.write', provider_tool_call_id: call, arguments: { path: `reports/${label}` } },
    });
    log.add('tool/completed', {
      turn: 't1',
      step: 'step-t1',
      actor: 'tool',
      payload: {
        tool_name: 'fs.write',
        provider_tool_call_id: call,
        status: 'completed',
        content: `wrote reports/${label}\n${locator}`,
        result_locator: locator,
        artifact_refs: [],
      },
    });
  }
  log.answer('t1', 'Three artifacts are ready.');
  log.save();
}

function seedApproval(session, approvalId, createdAt) {
  const log = builder(session);
  log.ask('t2', 'run the acceptance tests');
  log.add('approval/requested', {
    turn: 't2',
    step: 'step-t2',
    actor: 'scheduler',
    payload: {
      approval_id: approvalId,
      session_id: session,
      capability: 'process.spawn',
      tool_name: 'shell.run',
      args_summary: 'cargo test -p agent --test m4_acceptance',
      status: 'pending',
      created_at: createdAt,
    },
  });
  log.add('turn/yielded', { turn: 't2', actor: 'scheduler', payload: { approval_id: approvalId } });
  log.save();
}

function seedScratch() {
  const log = builder('scratch');
  log.ask('t1', 'scratch session');
  log.answer('t1', 'scratch answer');
  log.save();
}

/**
 * The incident shape (session `web-20261103090946-2ntt`): the turn answered,
 * a tool was refused, and the completion gate recorded `verdict=missing`. The
 * page must call this "已交付 · 未验证", never bare "完成".
 */
function seedUnverifiedDelivery() {
  const log = builder('unverified-delivery');
  log.add('run/started', { actor: 'scheduler' });
  log.ask('t1', '做一个 3 秒的火柴人战斗视频');
  log.add('tool/started', {
    turn: 't1',
    step: 'step-t1',
    actor: 'tool',
    payload: {
      tool_name: 'shell.run',
      provider_tool_call_id: 'call-unverified-1',
      arguments: { command: 'ffmpeg -version' },
    },
  });
  log.add('tool/failed', {
    turn: 't1',
    step: 'step-t1',
    actor: 'tool',
    payload: {
      tool_name: 'shell.run',
      provider_tool_call_id: 'call-unverified-1',
      status: 'failed',
      error_kind: 'sandbox_unavailable',
      content: 'policy denied tool shell.run: risk l3',
    },
  });
  log.add('workspace/changes.recorded', {
    turn: 't1',
    step: 'step-t1',
    payload: {
      changes: [
        {
          relative_path: 'partial.html',
          kind: 'modified',
          before_size: 10,
          after_size: 20,
          before_sha256: 'aaaa',
          after_sha256: 'bbbb',
        },
      ],
      changes_hash: 'partial-hash',
      complete: false,
      snapshot_id: 'snap-partial',
      task_id: 'task-partial',
      session_id: 'unverified-delivery',
    },
  });
  log.add('turn/completed', {
    turn: 't1',
    actor: 'scheduler',
    payload: { final_content: '交付已完成。', stop_reason: 'final_answer' },
  });
  log.add('completion/unverified_result_delivered', {
    turn: 't1',
    actor: 'scheduler',
    payload: { verdict: 'missing', task_revision: 4 },
  });
  log.add('run/completed', { actor: 'scheduler', payload: { stop_reason: 'final_answer' } });
  log.save();
}

/** A passing check covers the work: the one shape that may say "已验证完成". */
function seedVerifiedDelivery() {
  const log = builder('verified-delivery');
  log.add('run/started', { actor: 'scheduler' });
  log.ask('t1', '导出并验证一份报告');
  log.add('completion/checked', {
    turn: 't1',
    actor: 'scheduler',
    payload: { status: 'completed', kind: 'command', steps: 1 },
  });
  log.add('turn/completed', {
    turn: 't1',
    actor: 'scheduler',
    payload: { final_content: '报告已导出并通过检查。', stop_reason: 'final_answer' },
  });
  log.add('run/completed', { actor: 'scheduler', payload: { stop_reason: 'final_answer' } });
  log.save();
}

/**
 * A capability stop: the backend refused `process.spawn` twice, and the turn
 * ended as completed-but-incomplete with the three choices as its text. The
 * page must call this "未完成 · 已阻塞", never "已交付", and must name the
 * app-container route.
 */
function seedCapabilityBlocked() {
  const log = builder('capability-blocked');
  log.add('run/started', { actor: 'scheduler' });
  log.ask('t1', '做一个 3 秒的火柴人战斗视频');
  for (const [call, command] of [
    ['call-capability-1', 'ffmpeg -version'],
    ['call-capability-2', 'ffmpeg -f lavfi -i color=c=black:s=320x240:d=3 out.mp4'],
  ]) {
    log.add('tool/started', {
      turn: 't1',
      step: 'step-t1',
      actor: 'tool',
      payload: {
        tool_name: 'shell.run',
        provider_tool_call_id: call,
        arguments: { command },
      },
    });
    log.add('tool/failed', {
      turn: 't1',
      step: 'step-t1',
      actor: 'tool',
      payload: {
        tool_name: 'shell.run',
        provider_tool_call_id: call,
        status: 'failed',
        error_kind: 'permission_denied',
        outcome: 'gateway_refused',
        content:
          "policy denied tool shell.run: capability process.spawn is not provided by this build's execution sandbox",
        policy: {
          decision: 'deny',
          capabilities: ['process.spawn'],
          refusals: { 'process.spawn': 'sandbox_unavailable' },
          action_digest: 'sha256:capability-fixture',
        },
      },
    });
  }
  log.add('step/completed', { turn: 't1', step: 'step-t1', actor: 'agent', payload: {} });
  log.add('turn/completed', {
    turn: 't1',
    actor: 'scheduler',
    payload: {
      stop_reason: 'capability_blocked',
      incomplete: true,
      final_content:
        '能力缺口：`shell.run` 需要 `process.spawn`，当前后端不提供这个能力。已停止，不再用静态文件或页面假装完成。\n' +
        '请选择下一步：\n' +
        '1. 用 app-container 后端重启后重试；\n' +
        '2. 接受降级交付（只交付静态内容，并明确它不是原目标）；\n' +
        '3. 取消这个任务。\n' +
        '回复 1、2 或 3。',
    },
  });
  log.add('run/completed', { actor: 'scheduler', payload: { stop_reason: 'capability_blocked' } });
  log.save();
}

/**
 * A log captured from a real turn (`capture-real-chain.mjs`): the scheduler,
 * tool gateway, filesystem and `git diff` all ran. Replaying it keeps the
 * panels honest about real event shapes instead of this file's hand-written
 * fixtures.
 */
function seedRealChain() {
  const source = join(import.meta.dirname, '..', 'fixtures', 'real-chain', 'events.jsonl');
  const dir = join(dataDir, 'real-chain');
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'events.jsonl'), readFileSync(source));
}

/**
 * A log from the real `subagent.run` capture: a real closed turn, a real
 * in-process child and its settlement. The fixture is checked for shape in
 * `main`; the 子代理 tab is covered by the hand-written two-level tree.
 */


/** 100 turns × 10 items: the fold, not the API, is what gets measured. */function seedLong() {
  const log = builder('perf-1000');
  for (let turn = 1; turn <= 100; turn += 1) {
    const id = `t${turn}`;
    log.ask(id, `perf turn ${turn}`);
    log.add('reason/recorded', {
      turn: id,
      step: `step-${id}`,
      payload: reason(id, 0, 'situation', `turn ${turn} was asked`, `r/${turn}`),
    });
    for (let action = 1; action <= 6; action += 1) {
      const call = `c-${turn}-${action}`;
      log.add('tool/started', {
        turn: id,
        step: `step-${id}`,
        payload: { tool_name: 'fs.read', provider_tool_call_id: call, arguments: { path: `src/file-${turn}.rs` } },
      });
      log.add('tool/completed', {
        turn: id,
        step: `step-${id}`,
        actor: 'tool',
        payload: {
          tool_name: 'fs.read',
          provider_tool_call_id: call,
          status: 'completed',
          content: `line ${action} of file ${turn}`,
        },
      });
    }
    log.answer(id, `turn ${turn} done`);
  }
  log.save();
}


  seedWork();
  seedArtifacts();
  seedApproval('ui-approval', 'ap-smoke', nowIso());
  seedApproval('ui-expired', 'ap-old', new Date(Date.now() - 25 * 60 * 60_000).toISOString());
  seedScratch();
  seedUnverifiedDelivery();
  seedVerifiedDelivery();
  seedCapabilityBlocked();
  seedRealChain();
  seedLong();
}
