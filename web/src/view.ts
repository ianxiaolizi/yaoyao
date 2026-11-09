import type { ModelOption, PanelKey } from './types';
import { esc, fmtTokens, relativeTime, sessionStatusLabel, sessionStatusNote, turnStatus } from './format';
import { artifactLabel } from './artifacts';
import type { State, Store } from './state';
import { asRecord, clockText, groupsFor, refsFor, shortTitle, tracePopHtml, turnFacts, turnTitle, type Group } from './views/helpers';
import { currentTurnHtml, gateHtml, gateOf, pastRowHtml } from './views/transcript';
import { panelBody, sessionRow } from './views/evidence';
import { previewHtml } from './views/dock';

export interface Actions {
  selectSession(id: string): void;
  newSession(): void;
  send(text: string): Promise<void>;
  stop(): Promise<void>;
  decide(approvalId: string, decision: 'once' | 'turn' | 'session' | 'deny'): Promise<void>;
  resume(): Promise<void>;
  openPanel(key: PanelKey, turn?: string): void;
  selectArtifact(locator: string): void;
  exportSession(format: 'md' | 'json'): Promise<void>;
  fork(): Promise<void>;
  archive(archived: boolean): Promise<void>;
  remove(): Promise<void>;
  chooseModel(provider: string, model: string | null): Promise<void>;
  createSchedule(when: string, text: string): Promise<void>;
  cancelSchedule(itemId: string): Promise<void>;
}

const ICON = {
  panel: '<rect x="2" y="3" width="12" height="10" rx="2"/><path d="M6 3v10"/>',
  plus: '<path d="M8 3v10M3 8h10"/>',
  arrow: '<path d="M8 12.5v-9M4.2 7.2 8 3.4l3.8 3.8"/>',
  stop: '<rect x="5" y="5" width="6" height="6" rx="1" fill="currentColor" stroke="none"/>',
  layers: '<path d="M8 2.2 14 5.4 8 8.6 2 5.4 8 2.2Z"/><path d="M2 8.2 8 11.4l6-3.2"/><path d="M2 11 8 14.2 14 11"/>',
  close: '<path d="m4 4 8 8M12 4l-8 8"/>',
};

const svg = (path: string, size = 15): string =>
  `<svg width="${size}" height="${size}" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round">${path}</svg>`;

/** The sidebar's default row count; the rest sit behind "全部会话". */
const SESSION_RECENT = 6;

const PANEL_TABS: { key: PanelKey; label: string }[] = [
  { key: 'plan', label: '计划' },
  { key: 'diff', label: '改动' },
  { key: 'why', label: '推理链' },
  { key: 'context', label: '上下文' },
  { key: 'subagents', label: '子代理' },
  { key: 'schedules', label: '定时' },
];

export function mount(root: HTMLElement, store: Store, actions: Actions): void {
  root.innerHTML = `<div class="y-app">
    <aside class="y-side" id="side">
      <div class="y-side-head"><span class="mark">爻</span><button class="ibtn y-side-close" id="sideClose" type="button" aria-label="收起会话栏">${svg(ICON.close)}</button></div>
      <button class="y-new" id="newTask" type="button"><span class="plus">＋</span>新任务</button>
      <div class="y-side-scroll"><div class="y-sec">会话<span class="n" id="sessionCount"></span></div><div id="sessionList"></div></div>
      <div class="y-side-foot"><div class="y-foot-row">设置</div></div>
      <div class="y-rz y-rz-side" id="rzSide" role="separator" tabindex="0" aria-orientation="vertical" aria-label="调整会话栏宽度" title="拖动调整；双击复位"></div>
    </aside>
    <div class="y-side-mask" id="sideMask"></div>
    <main class="y-main">
      <header class="y-top">
        <button class="ibtn" id="toggleSide" type="button" aria-label="会话栏">${svg(ICON.panel, 16)}</button>
        <span class="y-title" id="title">爻爻</span>
        <span class="y-sep">/</span>
        <span class="y-round" id="round">第 0 轮</span>
        <span id="statusPill"></span>
        <span class="sp"></span>
        <button class="btn" id="details" type="button" aria-expanded="false">详情</button>
        <button class="ibtn" id="more" type="button" aria-label="更多">⋯</button>
      </header>
      <div class="y-status-note" id="statusNote" hidden></div>
      <div class="y-evpop" id="evpop" hidden>
        <div class="pop-h">本轮详情<span class="sp"></span><span class="esc">Esc</span></div>
        ${PANEL_TABS.map((tab) => `<button class="pop-item" type="button" data-evpanel="${tab.key}">${tab.label}<span class="v">›</span></button>`).join('')}
      </div>
      <div class="y-menu" id="menu" hidden>
        <button class="pop-item" type="button" data-menu="export-md">导出 Markdown</button>
        <button class="pop-item" type="button" data-menu="export-json">导出 JSON</button>
        <button class="pop-item" type="button" data-menu="fork">分叉这一轮</button>
        <button class="pop-item" type="button" data-menu="palette">命令面板 <span class="v">⌘K</span></button>
        <div class="pop-sep"></div>
        <button class="pop-item" type="button" data-menu="archive">归档此会话</button>
        <button class="pop-item danger" type="button" data-menu="remove">删除此会话</button>
      </div>
      <div class="y-stage">
        <div class="y-trace"><div class="y-ticks" id="trace"></div><div class="trace-pop" id="tracePop" role="tooltip" aria-hidden="true"></div></div>
        <div class="y-scroll" id="scroll"><div class="y-col" id="transcript"></div></div>
      </div>
      <aside class="y-sheet" id="sheet" aria-hidden="true" aria-label="本轮详情">
        <div class="y-rz y-rz-sheet" id="rzSheet" role="separator" tabindex="0" aria-orientation="vertical" aria-label="调整详情宽度" title="拖动调整；双击复位"></div>
        <div class="y-sheet-head"><span id="sheetTitle">本轮详情</span><span class="sp"></span><button class="ibtn" id="sheetClose" type="button" aria-label="关闭">${svg(ICON.close)}</button></div>
        <div class="y-sheet-tabs" role="tablist">
          ${PANEL_TABS.map((tab) => `<button type="button" role="tab" data-panel-tab="${tab.key}" aria-selected="false">${tab.label}</button>`).join('')}
        </div>
        <div class="y-sheet-body" id="sheetBody"></div>
      </aside>
      <div class="y-dock"><div class="y-dock-in">
        <section class="y-result" id="result" hidden>
          <div class="y-result-head" id="resultHead">
            <span class="y-result-t">本轮成果</span>
            <span class="y-result-n" id="resultCount"></span>
            <span class="sp"></span>
            <button class="y-result-more" id="resultFull" type="button" aria-label="全屏" aria-pressed="false">⤢</button>
            <button class="y-result-more" id="resultMore" type="button" aria-expanded="false">展开</button>
          </div>
          <div class="y-result-body">
            <div class="y-preview" id="artifactPreview"></div>
            <aside class="y-artifacts" id="artifactList"></aside>
          </div>
        </section>
        <div class="y-gate" id="gate"></div>
        <div class="y-composer">
          <div class="y-model-menu" id="modelMenu" role="menu" aria-label="选择模型" hidden></div>
          <div class="y-model-menu" id="policyMenu" role="menu" aria-label="审批策略" hidden></div>
          <div class="y-env-note" id="envNote" hidden></div>
          <textarea id="input" rows="1" placeholder="继续补充要求，或输入 / 命令…"></textarea>
          <div class="y-comp-row">
            <button class="y-chip" type="button" id="attach" aria-label="附件上传（v1.1）" title="附件上传留在 v1.1" disabled>${svg(ICON.plus, 13)}</button>
            <button class="y-chip" type="button" id="modelChip" aria-haspopup="menu" aria-expanded="false">模型</button>
            <button class="y-chip" type="button" id="policyChip" aria-haspopup="menu" aria-expanded="false" title="审批是逐个动作的门">审批：逐次</button>
            <span class="sp"></span>
            <button class="y-stop" id="stop" type="button" aria-label="停止" hidden>${svg(ICON.stop, 12)}</button>
            <button class="y-send" id="send" type="button" aria-label="发送">${svg(ICON.arrow, 14)}</button>
          </div>
        </div>
      </div></div>
    </main>
    <div class="y-toast" id="toast" role="status" hidden></div>
    <div class="y-palette" id="palette" hidden>
      <div class="y-palette-box" role="dialog" aria-label="命令面板">
        <div class="y-palette-input">${svg('<circle cx="7" cy="7" r="4.2"/><path d="m10.2 10.2 3.3 3.3"/>')}<input id="paletteInput" placeholder="搜索会话、动作、面板…" autocomplete="off" /><span class="kbd">Esc</span></div>
        <div class="y-palette-list" id="paletteList"></div>
        <div class="y-palette-foot">↑↓ 选择 · ↵ 打开 · Esc 关闭</div>
      </div>
    </div>
    <div class="y-auth" id="auth" hidden></div>
  </div>`;

  const $ = <T extends HTMLElement>(selector: string): T => root.querySelector(selector) as T;
  const app = $('.y-app');
  const transcript = $('#transcript');
  const scroll = $('#scroll');
  const gate = $('#gate');
  const toast = $('#toast');
  const input = $<HTMLTextAreaElement>('#input');
  const send = $<HTMLButtonElement>('#send');
  const stop = $<HTMLButtonElement>('#stop');
  const title = $('#title');
  const round = $('#round');
  const statusPill = $('#statusPill');
  const statusNote = $('#statusNote');
  const envNote = $('#envNote');
  const sessionList = $('#sessionList');
  const sessionCount = $('#sessionCount');
  const trace = $('#trace');
  const tracePop = $('#tracePop');
  const modelChip = $('#modelChip');
  const modelMenu = $('#modelMenu');
  const policyChip = $('#policyChip');
  const policyMenu = $('#policyMenu');
  const rzSide = $('#rzSide');
  const rzSheet = $('#rzSheet');
  const details = $('#details');
  const more = $('#more');
  const menu = $('#menu');
  const evpop = $('#evpop');
  const palette = $('#palette');
  const paletteInput = $<HTMLInputElement>('#paletteInput');
  const paletteList = $('#paletteList');
  const sideMask = $('#sideMask');
  const sheet = $('#sheet');
  const sheetClose = $('#sheetClose');
  const sheetBody = $('#sheetBody');
  const sheetTitle = $('#sheetTitle');
  const resultEl = $('#result');
  const resultHead = $('#resultHead');
  const resultMore = $('#resultMore');
  const resultFull = $('#resultFull');
  const resultCount = $('#resultCount');
  const artifactPreview = $('#artifactPreview');
  const artifactList = $('#artifactList');
  let panelTrigger: HTMLElement | null = null;
  let panelSignature = '';
  let resultSignature = '';
  let sidebarSignature = '';
  let traceSignature = '';
  let gateSignature = '';
  let lastActive: string | null = null;
  let lastGroups = 0;
  let toastTimer: ReturnType<typeof setTimeout> | undefined;

  // ── layout: the two widths a reader can drag ───────────────────────────
  interface WidthRule {
    variable: '--sidebar-w' | '--sheet-w';
    store: string;
    min: number;
    max: number;
    fallback: number;
    /** 1 when dragging right widens it; -1 when it grows to the left. */
    direction: 1 | -1;
  }

  function bindWidth(handle: HTMLElement, rule: WidthRule): void {
    // Storage is a convenience, not a requirement: a reader in a private window
    // still gets the width for this visit instead of a dead page.
    const read = (key: string): string | null => {
      try {
        return localStorage.getItem(key);
      } catch {
        return null;
      }
    };
    const write = (key: string, value: string | null): void => {
      try {
        if (value == null) localStorage.removeItem(key);
        else localStorage.setItem(key, value);
      } catch {
        // The width still applies through the CSS variable.
      }
    };
    const target = (): HTMLElement | null => handle.parentElement;
    const current = (): number =>
      Math.round(target()?.getBoundingClientRect().width || rule.fallback);
    const apply = (value: number | null): void => {
      if (value == null) {
        root.style.removeProperty(rule.variable);
        write(rule.store, null);
      } else {
        const width = Math.round(Math.min(rule.max, Math.max(rule.min, value)));
        root.style.setProperty(rule.variable, `${width}px`);
        write(rule.store, String(width));
      }
      handle.setAttribute('aria-valuenow', String(current()));
    };
    const stored = Number(read(rule.store));
    if (Number.isFinite(stored) && stored >= rule.min && stored <= rule.max) apply(stored);
    handle.setAttribute('aria-valuemin', String(rule.min));
    handle.setAttribute('aria-valuemax', String(rule.max));
    handle.setAttribute('aria-valuenow', String(current()));

    handle.addEventListener('dblclick', () => apply(null));
    handle.addEventListener('keydown', (event) => {
      if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
      event.preventDefault();
      if (event.key === 'Home') return apply(null);
      if (event.key === 'End') return apply(rule.max);
      const step = event.key === 'ArrowRight' ? 8 : -8;
      apply(current() + step * rule.direction);
    });
    handle.addEventListener('pointerdown', (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      const startWidth = current();
      const startX = event.clientX;
      handle.setPointerCapture(event.pointerId);
      document.body.classList.add('y-resizing');
      const move = (moveEvent: PointerEvent): void =>
        apply(startWidth + (moveEvent.clientX - startX) * rule.direction);
      const end = (): void => {
        document.body.classList.remove('y-resizing');
        handle.removeEventListener('pointermove', move);
        handle.removeEventListener('pointerup', end);
        handle.removeEventListener('pointercancel', end);
      };
      handle.addEventListener('pointermove', move);
      handle.addEventListener('pointerup', end);
      handle.addEventListener('pointercancel', end);
    });
  }

  bindWidth(rzSide, { variable: '--sidebar-w', store: 'yaoyao:layout:side', min: 196, max: 340, fallback: 236, direction: 1 });
  bindWidth(rzSheet, { variable: '--sheet-w', store: 'yaoyao:layout:sheet', min: 320, max: 560, fallback: 420, direction: -1 });

  // ── the model chip ─────────────────────────────────────────────────────
  function modelChoice(state: State): { provider: string; model?: string | null } | null {
    return state.summary?.model ?? state.chosenModel ?? null;
  }

  function modelLabel(state: State): string {
    const choice = modelChoice(state);
    if (choice) {
      const option = (state.meta?.models ?? []).find(
        (row) => row.provider === choice.provider && (row.model ?? '') === (choice.model ?? ''),
      );
      return option?.label ?? (choice.model || choice.provider);
    }
    return state.meta?.model || '模型';
  }

  function renderModelMenu(state: State): void {
    const options: ModelOption[] = state.meta?.models ?? [];
    const choice = modelChoice(state);
    if (!options.length) {
      modelMenu.innerHTML = '<div class="y-panel-empty">这个进程没有可切换的模型。</div>';
      return;
    }
    modelMenu.innerHTML =
      '<div class="pop-h">下一轮用哪个模型</div>' +
      options
        .map((option) => {
          const chosen = choice != null && choice.provider === option.provider && (choice.model ?? '') === (option.model ?? '');
          return `<button class="pop-item" type="button" role="menuitem" data-provider="${esc(option.provider)}" data-model="${esc(option.model ?? '')}">${chosen ? '✓ ' : ''}${esc(option.label)}</button>`;
        })
        .join('');
  }

  function closeModelMenu(): void {
    modelMenu.hidden = true;
    modelChip.setAttribute('aria-expanded', 'false');
  }

  modelChip.addEventListener('click', (event) => {
    event.stopPropagation();
    closePolicyMenu();
    if (!modelMenu.hidden) return closeModelMenu();
    renderModelMenu(store.get());
    modelMenu.hidden = false;
    modelChip.setAttribute('aria-expanded', 'true');
  });
  modelMenu.addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-provider]');
    if (!button?.dataset.provider) return;
    closeModelMenu();
    void actions.chooseModel(button.dataset.provider, button.dataset.model || null);
  });

  // ── the approval policy chip ───────────────────────────────────────────
  // This is deliberately a statement, not a switch. Approving is the moment a
  // person vouches for one exact action; an "auto approve" mode would remove
  // the person from that moment, which the permission work forbids.
  function closePolicyMenu(): void {
    policyMenu.hidden = true;
    policyChip.setAttribute('aria-expanded', 'false');
  }

  policyChip.addEventListener('click', (event) => {
    event.stopPropagation();
    closeModelMenu();
    if (!policyMenu.hidden) return closePolicyMenu();
    policyMenu.innerHTML =
      '<div class="pop-h">审批策略</div>' +
      '<button class="pop-item" type="button" role="menuitem" disabled>✓ 逐次确认</button>' +
      '<button class="pop-item" type="button" role="menuitem" disabled>自动批准（本版不提供）</button>' +
      '<div class="pop-note">审批是门：每个动作单独确认，高风险动作只允许一次。自动批准会绕过「有人在看」，所以不提供。</div>';
    policyMenu.hidden = false;
    policyChip.setAttribute('aria-expanded', 'true');
  });

  // ── the two-step delete ────────────────────────────────────────────────
  let removeTimer: ReturnType<typeof setTimeout> | undefined;
  function armRemove(button: HTMLElement): void {
    if (button.dataset.armed === '1') {
      clearTimeout(removeTimer);
      button.dataset.armed = '';
      button.textContent = '删除此会话';
      menu.hidden = true;
      void actions.remove();
      return;
    }
    button.dataset.armed = '1';
    button.textContent = '再点一次删除';
    clearTimeout(removeTimer);
    removeTimer = setTimeout(() => {
      button.dataset.armed = '';
      button.textContent = '删除此会话';
    }, 3000);
  }

  const resize = (): void => {
    input.style.height = 'auto';
    input.style.height = `${Math.min(160, input.scrollHeight)}px`;
  };

  input.addEventListener('input', () => {
    resize();
    send.disabled = !input.value.trim() || !stateHasToken() || Boolean(gateOf(store.get()));
  });
  input.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' && !event.shiftKey) {
      event.preventDefault();
      void submit();
    }
  });

  async function submit(): Promise<void> {
    const text = input.value.trim();
    if (!text) return;
    if (store.get().waiting || gateOf(store.get())) {
      store.dispatch({ type: 'toast', toast: '先处理上面的审批；这一轮会从那里继续' });
      return;
    }
    input.value = '';
    resize();
    send.disabled = true;
    await actions.send(text);
  }

  send.addEventListener('click', () => void submit());
  stop.addEventListener('click', () => void actions.stop());

  $('#toggleSide').addEventListener('click', () => {
    store.dispatch({ type: 'sidebar/toggle' });
  });
  $('#sideClose').addEventListener('click', () => store.dispatch({ type: 'sidebar/toggle' }));
  $('#newTask').addEventListener('click', () => actions.newSession());

  const lastTurn = (): string | undefined => {
    const groups = groupsFor(store.get().items);
    return groups[groups.length - 1]?.turn;
  };

  details.addEventListener('click', () => {
    evpop.hidden = true;
    if (store.get().panel) {
      store.dispatch({ type: 'panel/close' });
      return;
    }
    panelTrigger = details;
    actions.openPanel('plan');
  });
  sheetClose.addEventListener('click', () => store.dispatch({ type: 'panel/close' }));
  sheet.addEventListener('click', (event) => {
    const tab = (event.target as HTMLElement).closest<HTMLElement>('[data-panel-tab]');
    if (tab?.dataset.panelTab) {
      panelTrigger = details;
      const key = tab.dataset.panelTab as PanelKey;
      actions.openPanel(key, key === 'why' ? lastTurn() : undefined);
      return;
    }
    const add = (event.target as HTMLElement).closest<HTMLElement>('[data-schedule-add]');
    if (add) {
      const when = (sheetBody.querySelector('#schedWhen') as HTMLInputElement | null)?.value.trim() ?? '';
      const text = (sheetBody.querySelector('#schedText') as HTMLInputElement | null)?.value.trim() ?? '';
      if (!when || !text) {
        store.dispatch({ type: 'toast', toast: '时间和内容都要填' });
        return;
      }
      void actions.createSchedule(when, text);
      return;
    }
    const cancel = (event.target as HTMLElement).closest<HTMLElement>('[data-schedule-cancel]');
    if (cancel?.dataset.scheduleCancel) void actions.cancelSchedule(cancel.dataset.scheduleCancel);
  });

  let evTimer: ReturnType<typeof setTimeout> | undefined;
  const showEv = (): void => {
    if (store.get().panel) return;
    clearTimeout(evTimer);
    evpop.hidden = false;
  };
  const hideEv = (): void => {
    clearTimeout(evTimer);
    evTimer = setTimeout(() => {
      evpop.hidden = true;
    }, 140);
  };
  details.addEventListener('mouseenter', showEv);
  details.addEventListener('focus', showEv);
  details.addEventListener('mouseleave', hideEv);
  details.addEventListener('blur', hideEv);
  evpop.addEventListener('mouseenter', () => clearTimeout(evTimer));
  evpop.addEventListener('mouseleave', hideEv);
  evpop.addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-evpanel]');
    if (!button?.dataset.evpanel) return;
    evpop.hidden = true;
    const key = button.dataset.evpanel as PanelKey;
    actions.openPanel(key, key === 'why' ? lastTurn() : undefined);
  });

  more.addEventListener('click', (event) => {
    event.stopPropagation();
    evpop.hidden = true;
    menu.hidden = !menu.hidden;
  });
  document.addEventListener('click', (event) => {
    if (!menu.hidden && !menu.contains(event.target as Node) && !more.contains(event.target as Node)) menu.hidden = true;
    if (!evpop.hidden && !evpop.contains(event.target as Node) && !details.contains(event.target as Node)) {
      evpop.hidden = true;
    }
    if (!modelMenu.hidden && !modelMenu.contains(event.target as Node) && !modelChip.contains(event.target as Node)) {
      closeModelMenu();
    }
    if (!policyMenu.hidden && !policyMenu.contains(event.target as Node) && !policyChip.contains(event.target as Node)) {
      closePolicyMenu();
    }
  });
  menu.addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-menu]');
    if (!button?.dataset.menu) return;
    const action = button.dataset.menu;
    // Deleting asks twice, so the menu has to survive the first click: closing
    // it would hide the only button that can confirm the removal.
    if (action === 'remove') {
      armRemove(button);
      return;
    }
    menu.hidden = true;
    if (action === 'export-md') void actions.exportSession('md');
    else if (action === 'export-json') void actions.exportSession('json');
    else if (action === 'fork') void actions.fork();
    else if (action === 'palette') openPalette();
    else if (action === 'archive') void actions.archive(!store.get().summary?.archived);
  });

  sideMask.addEventListener('click', () => store.dispatch({ type: 'sidebar/toggle' }));

  const narrow = window.matchMedia('(max-width: 1080px)');
  narrow.addEventListener('change', (event) => {
    if (event.matches && store.get().sidebar) store.dispatch({ type: 'sidebar/toggle' });
    if (!event.matches && !store.get().sidebar) store.dispatch({ type: 'sidebar/toggle' });
  });
  resultHead.addEventListener('click', (event) => {
    if ((event.target as HTMLElement).closest('button')) return;
    store.dispatch({ type: 'result/toggle' });
  });
  resultMore.addEventListener('click', (event) => {
    event.stopPropagation();
    store.dispatch({ type: 'result/toggle' });
  });
  resultFull.addEventListener('click', (event) => {
    event.stopPropagation();
    store.dispatch({ type: 'result/full' });
  });
  artifactList.addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-artifact-locator]');
    if (button?.dataset.artifactLocator) actions.selectArtifact(button.dataset.artifactLocator);
  });

  sessionList.addEventListener('click', (event) => {
    const all = (event.target as HTMLElement).closest<HTMLElement>('[data-session-all]');
    if (all) {
      store.dispatch({ type: 'sessions/all' });
      return;
    }
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-session]');
    if (button?.dataset.session) actions.selectSession(button.dataset.session);
  });

  transcript.addEventListener('click', (event) => {
    const goal = (event.target as HTMLElement).closest<HTMLElement>('.y-goal');
    if (goal) {
      goal.classList.toggle('clamp');
      return;
    }
    const more = (event.target as HTMLElement).closest<HTMLElement>('[data-past-toggle]');
    if (more) {
      store.dispatch({ type: 'past/toggle' });
      return;
    }
    const toggle = (event.target as HTMLElement).closest<HTMLElement>('[data-turn-toggle]');
    if (toggle?.dataset.turnToggle) store.dispatch({ type: 'turn/toggle', turn: toggle.dataset.turnToggle });
    const why = (event.target as HTMLElement).closest<HTMLElement>('[data-why]');
    if (why?.dataset.why) {
      panelTrigger = why;
      actions.openPanel('why', why.dataset.why);
    }
  });

  gate.addEventListener('click', (event) => {
    const resume = (event.target as HTMLElement).closest<HTMLElement>('[data-resume]');
    if (resume) {
      gate.querySelectorAll<HTMLButtonElement>('button').forEach((button) => (button.disabled = true));
      void actions.resume();
      return;
    }
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-approval]');
    if (button?.dataset.approval && button.dataset.decision) {
      gate.querySelectorAll<HTMLButtonElement>('button').forEach((item) => (item.disabled = true));
      void actions.decide(button.dataset.approval, button.dataset.decision as 'once' | 'turn' | 'session' | 'deny');
    }
  });

  trace.addEventListener('click', (event) => {
    const tick = (event.target as HTMLElement).closest<HTMLElement>('[data-jump]');
    if (!tick?.dataset.jump) return;
    const target = transcript.querySelector<HTMLElement>(`[data-turn="${CSS.escape(tick.dataset.jump)}"]`);
    target?.scrollIntoView({ block: 'start', behavior: 'smooth' });
  });

  // The rail is a scrubber: hover/focus previews one turn in place, Alt↑↓
  // moves the preview, Enter (a click) jumps. Nothing here changes layout.
  function showTracePop(tick: HTMLElement, group: Group): void {
    tracePop.innerHTML = tracePopHtml(group);
    tracePop.classList.add('is-on');
    tracePop.setAttribute('aria-hidden', 'false');
    tracePop.dataset.turn = group.turn;
    const height = tracePop.offsetHeight;
    const room = trace.parentElement?.clientHeight ?? 0;
    const top = Math.max(4, Math.min(tick.offsetTop - 10, room - height - 8));
    tracePop.style.top = `${top}px`;
  }

  function hideTracePop(): void {
    tracePop.classList.remove('is-on');
    tracePop.setAttribute('aria-hidden', 'true');
    delete tracePop.dataset.turn;
  }

  function traceTickAt(node: EventTarget | null): HTMLElement | null {
    return node instanceof HTMLElement ? node.closest<HTMLElement>('[data-jump]') : null;
  }

  function traceGroup(turn: string): Group | undefined {
    return groupsFor(store.get().items).find((group) => group.turn === turn);
  }

  trace.addEventListener('mouseover', (event) => {
    const tick = traceTickAt(event.target);
    const jump = tick?.dataset.jump;
    const group = jump ? traceGroup(jump) : undefined;
    if (tick && group) showTracePop(tick, group);
  });
  trace.addEventListener('mouseleave', hideTracePop);
  trace.addEventListener('focusin', (event) => {
    const tick = traceTickAt(event.target);
    const jump = tick?.dataset.jump;
    const group = jump ? traceGroup(jump) : undefined;
    if (tick && group) showTracePop(tick, group);
  });
  trace.addEventListener('focusout', (event) => {
    if (!trace.contains(event.relatedTarget as Node | null)) hideTracePop();
  });

  document.addEventListener('keydown', (event) => {
    if (!event.altKey || (event.key !== 'ArrowUp' && event.key !== 'ArrowDown')) return;
    const target = event.target as HTMLElement | null;
    if (target?.closest('input, textarea, select, [contenteditable="true"]')) return;
    const ticks = [...trace.querySelectorAll<HTMLElement>('.y-tick')];
    if (!ticks.length) return;
    event.preventDefault();
    const focused = document.activeElement instanceof HTMLElement ? ticks.indexOf(document.activeElement as HTMLElement) : -1;
    const previewed = tracePop.dataset.turn ? ticks.findIndex((tick) => tick.dataset.jump === tracePop.dataset.turn) : -1;
    let index = focused >= 0 ? focused : previewed >= 0 ? previewed : ticks.length - 1;
    index = Math.max(0, Math.min(ticks.length - 1, index + (event.key === 'ArrowDown' ? 1 : -1)));
    const tick = ticks[index];
    tick.focus({ preventScroll: true });
    const group = traceGroup(tick.dataset.jump ?? '');
    if (group) showTracePop(tick, group);
  });

  document.addEventListener('keydown', (event) => {
    if (event.key !== 'Escape') return;
    if (!palette.hidden) {
      event.preventDefault();
      closePalette();
    } else if (!menu.hidden) {
      event.preventDefault();
      menu.hidden = true;
    } else if (!modelMenu.hidden) {
      event.preventDefault();
      closeModelMenu();
    } else if (!policyMenu.hidden) {
      event.preventDefault();
      closePolicyMenu();
    } else if (!evpop.hidden) {
      event.preventDefault();
      evpop.hidden = true;
    } else if (store.get().panel) {
      event.preventDefault();
      store.dispatch({ type: 'panel/close' });
    } else if (store.get().resultFull) {
      event.preventDefault();
      store.dispatch({ type: 'result/full' });
    } else if (store.get().result) {
      event.preventDefault();
      store.dispatch({ type: 'result/close' });
    } else if (!store.get().sidebar) {
      store.dispatch({ type: 'sidebar/toggle' });
    }
  });

  function stateHasToken(): boolean {
    return location.hash.startsWith('#t=');
  }

  interface Command {
    label: string;
    hint?: string;
    group: string;
    run: () => void | Promise<void>;
  }

  let paletteCommands: Command[] = [];
  let paletteIndex = 0;
  let paletteTrigger: HTMLElement | null = null;

  function buildCommands(query: string): Command[] {
    const state = store.get();
    const commands: Command[] = [
      { group: '动作', label: '新任务', hint: '从一句话开始', run: () => actions.newSession() },
      { group: '动作', label: '导出 Markdown', hint: '当前会话', run: () => actions.exportSession('md') },
      { group: '动作', label: '导出 JSON', hint: '当前会话', run: () => actions.exportSession('json') },
      { group: '动作', label: '分叉这一轮', run: () => actions.fork() },
      ...PANEL_TABS.map((tab) => ({
        group: '面板',
        label: `打开${tab.label}`,
        run: () => actions.openPanel(tab.key),
      })),
      ...state.sessions.slice(0, 8).map((row) => ({
        group: '会话',
        label: row.title || row.id,
        hint: relativeTime(row.last_activity_at),
        run: () => actions.selectSession(row.id),
      })),
    ];
    if (state.running) commands.splice(4, 0, { group: '动作', label: '停止这一轮', run: () => actions.stop() });
    const needle = query.trim().toLowerCase();
    if (!needle) return commands;
    return commands.filter((command) => command.label.toLowerCase().includes(needle));
  }

  function renderPalette(): void {
    const query = paletteInput.value;
    paletteCommands = buildCommands(query);
    paletteIndex = Math.max(0, Math.min(paletteIndex, paletteCommands.length - 1));
    if (!paletteCommands.length) {
      paletteList.innerHTML = '<div class="y-palette-empty">没有匹配的命令</div>';
      return;
    }
    let html = '';
    let lastGroup = '';
    paletteCommands.forEach((command, index) => {
      if (command.group !== lastGroup) {
        html += `<div class="y-cmd-group">${command.group}</div>`;
        lastGroup = command.group;
      }
      html += `<button class="y-cmd ${index === paletteIndex ? 'is-on' : ''}" type="button" data-cmd="${index}"><span>${esc(command.label)}</span>${command.hint ? `<span class="hint">${esc(command.hint)}</span>` : ''}</button>`;
    });
    paletteList.innerHTML = html;
  }

  function openPalette(): void {
    paletteTrigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    evpop.hidden = true;
    menu.hidden = true;
    palette.hidden = false;
    paletteInput.value = '';
    paletteIndex = 0;
    renderPalette();
    requestAnimationFrame(() => paletteInput.focus());
  }

  function closePalette(): void {
    palette.hidden = true;
    paletteTrigger?.focus();
  }

  function runCommand(command: Command): void {
    closePalette();
    void Promise.resolve(command.run()).catch((error) => store.dispatch({ type: 'toast', toast: (error as Error).message }));
  }

  paletteList.addEventListener('click', (event) => {
    const button = (event.target as HTMLElement).closest<HTMLElement>('[data-cmd]');
    if (!button?.dataset.cmd) return;
    const command = paletteCommands[Number(button.dataset.cmd)];
    if (command) runCommand(command);
  });
  paletteInput.addEventListener('input', () => {
    paletteIndex = 0;
    renderPalette();
  });
  paletteInput.addEventListener('keydown', (event) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      const delta = event.key === 'ArrowDown' ? 1 : -1;
      paletteIndex = Math.max(0, Math.min(paletteCommands.length - 1, paletteIndex + delta));
      renderPalette();
      paletteList.querySelector<HTMLElement>('.is-on')?.scrollIntoView({ block: 'nearest' });
    } else if (event.key === 'Enter') {
      event.preventDefault();
      const command = paletteCommands[paletteIndex];
      if (command) runCommand(command);
    } else if (event.key === 'Escape') {
      event.preventDefault();
      closePalette();
    }
  });
  palette.addEventListener('click', (event) => {
    if (event.target === palette) closePalette();
  });

  document.addEventListener('keydown', (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') {
      event.preventDefault();
      if (palette.hidden) openPalette();
      else closePalette();
    }
  });

  function renderSidebar(state: State): void {
    const signature = `${state.active}|${state.sessionAll}|${state.sessions.map((row) => `${row.id}:${row.title}:${row.pending_approvals}:${row.archived ? 1 : 0}`).join('|')}`;
    if (signature === sidebarSignature) return;
    sidebarSignature = signature;
    sessionCount.textContent = String(state.sessions.length);
    const live = state.sessions.filter((row) => !row.archived);
    // One workspace per process today, so this is a recency fold rather than a
    // per-workspace tree: six rows, the active one always kept in view, and the
    // rest one click away. See plan/06 §4 for the deferred grouping decision.
    const folded = !state.sessionAll && live.length > SESSION_RECENT;
    let visible = live;
    if (folded) {
      visible = live.slice(0, SESSION_RECENT);
      const active = live.find((row) => row.id === state.active);
      if (active && !visible.some((row) => row.id === active.id)) {
        visible = [...visible.slice(0, SESSION_RECENT - 1), active];
      }
    }
    const archived = state.sessions.filter((row) => row.archived);
    const rows =
      visible.map((row) => sessionRow(row, row.id === state.active)).join('') ||
      '<div class="y-sec" style="margin:8px 6px">还没有会话</div>';
    const more =
      live.length > SESSION_RECENT
        ? `<button class="y-sec-more" type="button" data-session-all>${state.sessionAll ? '收起' : `全部会话 · ${live.length}`}</button>`
        : '';
    // Archived sessions are put away, not older: their own section, at the
    // bottom, still reachable in one click.
    const archivedRows = archived.length
      ? `<div class="y-sec y-sec-arch">已归档<span class="n">${archived.length}</span></div>${archived
          .map((row) => sessionRow(row, row.id === state.active))
          .join('')}`
      : '';
    sessionList.innerHTML = rows + more + archivedRows;
  }

  function renderTranscript(state: State): void {
    const groups = groupsFor(state.items);
    const switched = state.active !== lastActive || lastGroups === 0;
    const nearBottom = !switched && scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 120;
    const open = new Set(
      [...transcript.querySelectorAll<HTMLDetailsElement>('details[open]')].map((node) => node.dataset.key ?? ''),
    );
    if (!state.active) {
      lastActive = null;
      lastGroups = 0;
      transcript.innerHTML = `<div class="y-empty"><span class="mark">爻</span><h3>从左侧选一个会话</h3><p>或者点「新任务」，从一句话开始。</p></div>`;
      return;
    }
    if (!groups.length) {
      lastActive = state.active;
      lastGroups = 0;
      transcript.innerHTML = `<div class="y-empty"><span class="mark">爻</span><h3>新任务</h3><p>${state.error ? esc(state.error) : '说一句话，爻爻会把它记进同一份会话日志。'}</p></div>`;
      return;
    }
    const current = groups[groups.length - 1];
    const past = groups.slice(0, -1);
    const folded = !state.showAllPast && past.length > 2 ? past.slice(0, -2) : [];
    const visiblePast = folded.length ? past.slice(-2) : past;
    const more = folded.length
      ? `<button class="y-past-more" type="button" data-past-toggle>更早的 ${folded.length} 轮 ⌄</button>`
      : state.showAllPast && past.length > 2
        ? '<button class="y-past-more" type="button" data-past-toggle>收起更早的轮次 ⌃</button>'
        : '';
    const pastHtml = visiblePast.map((group) => pastRowHtml(group, state.expanded.has(group.turn))).join('');
    transcript.innerHTML = `<div class="y-past">${more}${pastHtml}</div>${currentTurnHtml(current, state)}`;
    for (const key of open) {
      if (!key) continue;
      const node = transcript.querySelector<HTMLDetailsElement>(`details[data-key="${CSS.escape(key)}"]`);
      if (node) node.open = true;
    }
    lastActive = state.active;
    lastGroups = groups.length;
    if (switched) scroll.scrollTop = 0;
    else if (nearBottom) scroll.scrollTop = scroll.scrollHeight;
  }

  function renderTrace(state: State): void {
    const groups = groupsFor(state.items);
    const signature = groups.map((group) => `${group.turn}:${group.end?.ending ?? ''}`).join('|');
    if (signature === traceSignature) return;
    traceSignature = signature;
    hideTracePop();
    const host = trace.parentElement;
    if (host) host.hidden = groups.length < 3;
    trace.innerHTML = groups
      .map((group, index) => {
        const status = turnStatus(group.end);
        const cls = index === groups.length - 1 ? 'now' : group.end?.ending === 'yielded' ? 'wait' : '';
        const { steps } = turnFacts(group);
        const label = `第 ${group.index} 轮 · ${shortTitle(turnTitle(group), 40)} · ${status.label} · ${steps} 步`;
        return `<button class="y-tick ${cls}" type="button" data-jump="${esc(group.turn)}" aria-label="${esc(label)}" aria-describedby="tracePop"></button>`;
      })
      .join('');
  }

  function renderGate(state: State): void {
    const approval = gateOf(state);
    const signature = approval ? `${approval.id}:${approval.state}` : '';
    if (signature === gateSignature) return;
    gateSignature = signature;
    gate.innerHTML = gateHtml(state);
    // The gate is the card, not the condition: a session can be "waiting" (its
    // summary still says yielded) with no card left to show, and reserving an
    // empty box there reads as a control that failed to load. The status pill
    // already says 等待确认.
    gate.hidden = !approval;
  }

  function renderPanel(state: State, previous: State): void {
    const panel = state.panel;
    sheet.classList.toggle('open', Boolean(panel));
    sheet.setAttribute('aria-hidden', String(!panel));
    details.setAttribute('aria-expanded', String(Boolean(panel)));
    if (!panel) {
      panelSignature = '';
      if (previous.panel && panelTrigger) {
        panelTrigger.focus();
        panelTrigger = null;
      }
      return;
    }
    evpop.hidden = true;
    if (!previous.panel) sheetClose.focus();
    const label = PANEL_TABS.find((tab) => tab.key === panel.key)?.label ?? '本轮详情';
    const turn = typeof asRecord(panel.data).turn === 'string' ? String(asRecord(panel.data).turn) : '';
    const index = turn ? groupsFor(state.items).findIndex((group) => group.turn === turn) : -1;
    sheetTitle.textContent = panel.key === 'why' && index >= 0 ? `${label} · 第 ${index + 1} 轮` : label;
    sheet.querySelectorAll<HTMLElement>('[data-panel-tab]').forEach((tab) => {
      tab.setAttribute('aria-selected', String(tab.dataset.panelTab === panel.key));
    });
    const signature = `${panel.key}:${panel.loading}:${panel.error ?? ''}:${panel.data ? 'data' : ''}`;
    if (signature === panelSignature) return;
    panelSignature = signature;
    sheetBody.innerHTML = panelBody(panel);
  }

  function renderResult(state: State): void {
    const groups = groupsFor(state.items);
    const currentTurn = groups[groups.length - 1]?.turn;
    const refs = refsFor(state.items).filter((ref) => !ref.turn || ref.turn === currentTurn);
    if (!refs.length) {
      resultEl.hidden = true;
      resultSignature = '';
      return;
    }
    resultEl.hidden = false;
    resultEl.classList.toggle('open', state.result);
    resultEl.classList.toggle('full', state.resultFull);
    resultMore.textContent = state.result ? '收起' : '展开';
    resultMore.setAttribute('aria-expanded', String(state.result));
    resultFull.setAttribute('aria-pressed', String(state.resultFull));
    resultCount.textContent = `${refs.length} 个产出`;
    const signature = `${refs.map((ref) => ref.locator).join('|')}|${state.artifact}|${state.artifactView?.kind ?? ''}`;
    if (signature === resultSignature) return;
    resultSignature = signature;
    const turnIndex = new Map(groups.map((group, index) => [group.turn, index + 1]));
    artifactList.innerHTML =
      refs
        .map(
          (ref) =>
            `<button class="y-artifact ${ref.locator === state.artifact ? 'on' : ''}" type="button" data-artifact-locator="${esc(ref.locator)}"><span>${esc(ref.label ?? artifactLabel(ref.locator))}</span><span>${esc(ref.turn && turnIndex.has(ref.turn) ? `第 ${turnIndex.get(ref.turn)} 轮` : '')}</span></button>`,
        )
        .join('') + (state.summary?.usage ? `<div class="y-art-use">${fmtTokens(state.summary.usage.total_tokens ?? (state.summary.usage.input_tokens ?? 0) + (state.summary.usage.output_tokens ?? 0))} tokens</div>` : '');
    artifactPreview.innerHTML = state.artifact
      ? state.artifactView
        ? previewHtml(state.artifactView)
        : '<div class="y-panel-empty">读取 artifact…</div>'
      : '<div class="y-panel-empty">选择一个产出…</div>';
  }

  function render(state: State, previous: State): void {
    const groups = groupsFor(state.items);
    const current = groups[groups.length - 1];
    const status = turnStatus(current?.end);
    const pending = [...state.items.values()].some((item) => item.kind === 'approval' && item.state === 'pending');

    app.classList.toggle('y-side-off', !state.sidebar);
    app.classList.toggle('y-side-open', state.sidebar);
    title.textContent = state.summary?.title || (state.active ? '新任务' : '爻爻');
    round.textContent = `第 ${groups.length} 轮`;
    modelChip.textContent = modelLabel(state);
    modelChip.title = '这个会话用哪个模型；选择在下一轮生效';
    const archived = Boolean(state.summary?.archived);
    const archiveItem = menu.querySelector<HTMLElement>('[data-menu="archive"]');
    if (archiveItem && archiveItem.dataset.armed !== '1') {
      archiveItem.textContent = archived ? '恢复此会话' : '归档此会话';
    }
    // The top pill reports the kernel's fold for the latest session, not the
    // turn's ending. Only a session with a current turn may carry a claim.
    const sessionStatus = current ? state.status : null;
    const sessionLabel = sessionStatusLabel(sessionStatus);
    statusPill.innerHTML = state.running
      ? '<span class="pill run"><i></i>运行中</span>'
      : state.waiting || pending
        ? '<span class="pill wait"><i></i>等待确认</span>'
        : current
          ? `<span class="pill ${sessionStatus ? sessionLabel.className : status.className}"><i></i>${sessionStatus ? sessionLabel.label : status.label}</span>`
          : '';

    // A run in flight has no verdict yet: the axes say "unverified" merely
    // because nothing has been checked, so the note waits for the terminal event.
    const note =
      current && !state.running && !state.waiting && !pending
        ? sessionStatusNote(sessionStatus)
        : null;
    statusNote.hidden = !note;
    statusNote.textContent = note ?? '';
    statusNote.className = note
      ? `y-status-note ${sessionLabel.className === 'unverified' ? 'warn' : 'fail'}`
      : 'y-status-note';

    const local = state.meta?.sandbox === 'local';
    envNote.hidden = !local;
    envNote.textContent = local
      ? '当前是 local 后端：爻爻可以编辑文件，但不会启动命令或渲染进程。需要构建、测试、导出视频时，请用 app-container 启动。'
      : '';
    stop.hidden = !state.running;
    send.disabled = !stateHasToken() || !input.value.trim() || Boolean(gateOf(state));
    details.title = '证据面板（M5）';

    renderSidebar(state);
    renderTrace(state);
    renderGate(state);
    renderPanel(state, previous);
    renderResult(state);

    if (state.items !== previous.items || state.active !== previous.active || state.expanded !== previous.expanded || state.showAllPast !== previous.showAllPast || state.running !== previous.running) {
      renderTranscript(state);
    } else if (state.live && state.live !== previous.live) {
      const live = transcript.querySelector<HTMLElement>('.y-live');
      if (live) live.innerHTML = `${esc(state.live.text ?? '')}<span class="cursor"></span>`;
      else renderTranscript(state);
    }

    toast.hidden = !state.toast;
    toast.textContent = state.toast ?? '';
    toast.classList.toggle('error', Boolean(state.error && state.toast === state.error));
    if (state.toast) {
      clearTimeout(toastTimer);
      toastTimer = setTimeout(() => store.dispatch({ type: 'toast', toast: null }), 2200);
    }
  }

  store.subscribe(render);
  render(store.get(), { ...store.get(), items: new Map(), expanded: new Set() });
  resize();

  // The gate counts down on the page's clock, from the expiry the projection
  // recorded. Nothing is re-requested: the card's own state is the server's,
  // and this only changes the words on its clock.
  setInterval(() => {
    const node = gate.querySelector<HTMLElement>('[data-expires]');
    if (!node) return;
    const expires = node.dataset.expires ?? '';
    const left = expires ? Date.parse(expires) - Date.now() : 0;
    const text = left > 0 ? `${clockText(left)} 后失效` : '已过期';
    if (node.textContent !== text) node.textContent = text;
  }, 1000);
}

export function renderAuth(root: HTMLElement): void {
  root.innerHTML = `<div class="y-auth"><div class="card">
    <span class="mark" style="width:36px;height:36px;border-radius:9px;font-size:17px;display:grid;place-items:center">爻</span>
    <h1>缺少访问令牌</h1>
    <p>用 <code>agent web</code> 打印的完整 URL 打开，令牌在 <code>#t=</code> 后面；页面只把它放进 <code>Authorization</code> 头，不写存储。</p>
  </div></div>`;
}
