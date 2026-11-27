(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const appWindow = window.__TAURI__.window.getCurrentWindow();

  let cfg = null;
  let recState = null;
  let mics = [];
  let capturingHotkey = false;
  let draggingSlider = false;
  let saveTimer = null;
  let uiReady = false;
  let resizeToken = 0;
  let activeFitHeight = null;
  let heightVerifyToken = 0;
  let wasMaximized = false;

  let activeTab = 'clips';
  let clips = [];
  let clipsLoadedOnce = false;
  let clipsLoadedDir = null;
  let selectMode = false;
  let selectedClipPaths = new Set();
  let selectionAnchorIdx = null;
  let clipPreviewCache = new Map();
  const PREVIEW_CACHE_LIMIT = 200;
  let queuedPreviewKeys = new Set();
  let previewQueue = [];
  let activePreviewWorkers = 0;
  let thumbObserver = null;
  let cardMap = new Map();
  let cardRefs = new WeakMap();
  let renameTargetClip = null;
  let deleteTargetPaths = [];
  let clipsRefreshToken = 0;
  let sharingPaths = new Set();
  // Last share settings the clip cards were rendered against, so toggling R2 on/off can refresh them.
  let renderedShareConfig = null;

  const $ = (id) => document.getElementById(id);

  function fmtSecs(s) {
    const m = Math.floor(s / 60);
    const rem = s % 60;
    if (m === 0) return `${rem} s`;
    if (rem === 0) return `${m} min`;
    return `${m} min ${rem} s`;
  }

  function fmtCount(n) {
    return Number(n || 0).toLocaleString('en-US');
  }

  function updateRangeFill(el) {
    const min = Number(el.min || 0);
    const max = Number(el.max || 100);
    const val = Number(el.value || 0);
    const pct = max > min ? ((val - min) / (max - min)) * 100 : 0;
    el.style.setProperty('--pct', `${pct}%`);
  }

  function showToast(kind, title, message) {
    const area = $('toast-area');
    const card = document.createElement('div');
    card.className = `toast is-${kind || 'info'}`;

    const t = document.createElement('div');
    t.className = 'toast-title';
    t.textContent = title || 'clip';

    const m = document.createElement('div');
    m.className = 'toast-msg';
    m.textContent = message || '';

    card.appendChild(t);
    if (message) card.appendChild(m);
    area.appendChild(card);

    setTimeout(() => {
      card.remove();
    }, 3600);
  }

  function measureTargetWindowHeight() {
    const shell = document.querySelector('.app-shell');
    const titlebar = document.querySelector('.titlebar');
    const clipsToolbar = $('clips-toolbar');
    const statusBar = document.querySelector('.status-bar');
    const contentScroll = document.querySelector('.content-scroll');
    const activePage = document.querySelector('.page.is-visible');
    if (!shell || !titlebar || !statusBar || !contentScroll || !activePage) return null;

    const csStyle = getComputedStyle(contentScroll);
    const shellStyle = getComputedStyle(shell);
    const padY = parseFloat(csStyle.paddingTop) + parseFloat(csStyle.paddingBottom);
    const borderY = parseFloat(shellStyle.borderTopWidth) + parseFloat(shellStyle.borderBottomWidth);
    const pageH = activePage.getBoundingClientRect().height;
    const toolbarH = clipsToolbar ? clipsToolbar.getBoundingClientRect().height : 0;
    const chromeH = titlebar.getBoundingClientRect().height + toolbarH + statusBar.getBoundingClientRect().height;

    const raw = Math.ceil(chromeH + borderY + padY + pageH);
    if (activePage.id === 'page-clips') {
      return Math.min(Math.max(raw, 280), 621);
    }
    return raw;
  }

  async function syncWindowHeight(animate) {
    if (!uiReady) return;
    // Until the first list_clips resolves the Clips page is still empty, so fitting it would
    // shrink the window to the empty-state height right before the gallery arrives.
    if (activeTab === 'clips' && !clipsLoadedOnce) return;
    const targetHeight = measureTargetWindowHeight();
    if (!targetHeight) return;
    if (Math.abs(window.innerHeight - targetHeight) <= 1) return;
    // An animated fit reports intermediate heights while it runs; re-measuring must not restart
    // it when the target has not changed.
    if (targetHeight === activeFitHeight) return;

    activeFitHeight = targetHeight;
    const token = ++resizeToken;
    const contentScroll = document.querySelector('.content-scroll');
    if (animate && contentScroll) {
      contentScroll.classList.add('is-animating-height');
    }
    try {
      await invoke('fit_window_height', { height: targetHeight, animate: Boolean(animate) });
    } catch (_) {
    } finally {
      if (token === resizeToken) {
        activeFitHeight = null;
        if (contentScroll) contentScroll.classList.remove('is-animating-height');
      }
    }
  }

  // Cards inserted right before a measurement may not have been laid out yet, so re-measure once
  // after paint. syncWindowHeight refuses to refit the height already being animated to, so this
  // only corrects a genuinely changed target and never restarts an accurate fit.
  function verifyClipsWindowHeight() {
    const token = ++heightVerifyToken;
    requestAnimationFrame(() => {
      requestAnimationFrame(() => {
        if (token !== heightVerifyToken || activeTab !== 'clips' || !clipsLoadedOnce) return;
        syncWindowHeight(true);
      });
    });
  }

  async function switchTab(name) {
    activeTab = name;
    const tabs = ['clips', 'settings'];
    for (const t of tabs) {
      const isCurrent = t === name;
      $(`tab-${t}`).classList.toggle('is-active', isCurrent);
      $(`tab-${t}`).setAttribute('aria-selected', String(isCurrent));
      $(`page-${t}`).classList.toggle('is-visible', isCurrent);
    }
    $('clips-toolbar')?.classList.toggle('is-hidden', name !== 'clips');
    document.querySelector('.content-scroll')?.classList.toggle('is-clips-tab', name === 'clips');

    hideContextMenu();

    if (capturingHotkey) {
      cancelCapture();
    }
    if (name === 'clips') {
      const curDir = cfg ? cfg.output_dir : null;
      if (clipsLoadedDir !== curDir) {
        resetClipsView();
      }
      if (!clipsLoadedOnce) {
        await refreshClips();
        return;
      }
      observePendingPreviews();
      syncWindowHeight(true);
      refreshClips({ silent: true });
      return;
    }
    syncWindowHeight(true);
  }

  function fmtBytes(b) {
    const n = Number(b || 0);
    if (n >= 1e9) return `${(n / 1e9).toFixed(2)} GB`;
    if (n >= 1e6) return `${Math.max(1, Math.round(n / 1e6))} MB`;
    if (n >= 1e3) return `${Math.max(1, Math.round(n / 1e3))} KB`;
    return `${n} B`;
  }

  function fmtDurationMs(ms) {
    if (ms == null || !Number.isFinite(ms)) return null;
    const totalSecs = Math.max(0, Math.round(ms / 1000));
    const h = Math.floor(totalSecs / 3600);
    const m = Math.floor((totalSecs % 3600) / 60);
    const s = totalSecs % 60;
    if (h > 0) return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
    return `${m}:${String(s).padStart(2, '0')}`;
  }

  function fmtShortDate(ms) {
    if (!ms) return '';
    try {
      return new Date(ms).toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
    } catch (_) {
      return '';
    }
  }

  function clipCacheKey(c) {
    return `${c.path}|${c.size_bytes}|${c.modified_ms}`;
  }

  function getCachedPreview(c) {
    const key = clipCacheKey(c);
    const hit = clipPreviewCache.get(key);
    if (!hit) return null;
    // Re-insert to keep Map insertion order as a cheap LRU.
    clipPreviewCache.delete(key);
    clipPreviewCache.set(key, hit);
    return hit;
  }

  function prunePreviewCache() {
    while (clipPreviewCache.size > PREVIEW_CACHE_LIMIT) {
      const oldest = clipPreviewCache.keys().next();
      if (oldest.done) break;
      clipPreviewCache.delete(oldest.value);
    }
  }

  function dropPreviewForPath(path) {
    const prefix = `${path}|`;
    for (const key of Array.from(clipPreviewCache.keys())) {
      if (key.startsWith(prefix)) clipPreviewCache.delete(key);
    }
  }

  function currentClipForCard(card) {
    const idx = Number(card.dataset.index);
    return clips[idx] || null;
  }

  function clipMetaText(c) {
    const cached = getCachedPreview(c);
    let h = cached?.height ?? c.height;
    if (h === 1088) h = 1080;
    const parts = [fmtBytes(c.size_bytes)];
    if (h) parts.push(`${h}p`);
    const d = fmtShortDate(c.modified_ms);
    if (d) parts.push(d);
    return parts.join(' · ');
  }

  function updateClipsToolbar() {
    const page = $('page-clips');
    const isSelecting = selectMode || selectedClipPaths.size > 0;
    page.classList.toggle('is-selecting', isSelecting);

    $('clips-actions-normal').classList.toggle('is-hidden', isSelecting);
    $('clips-actions-select').classList.toggle('is-hidden', !isSelecting);
    $('btn-clips-select').disabled = clips.length === 0;

    if (isSelecting) {
      const selCount = selectedClipPaths.size;
      const selBytes = clips
        .filter((c) => selectedClipPaths.has(c.path))
        .reduce((sum, c) => sum + (c.size_bytes || 0), 0);
      $('clips-summary').textContent = selCount > 0
        ? `${fmtCount(selCount)} selected · ${fmtBytes(selBytes)}`
        : 'Select clips to delete';

      const allSelected = clips.length > 0 && selCount === clips.length;
      $('btn-clips-select-all').textContent = allSelected ? 'Deselect all' : 'Select all';
      $('btn-clips-select-all').disabled = clips.length === 0;
      const delBtn = $('btn-clips-delete-bulk');
      delBtn.disabled = selCount === 0;
      delBtn.textContent = selCount > 0 ? `Delete (${fmtCount(selCount)})` : 'Delete';
    } else {
      const totalBytes = clips.reduce((sum, c) => sum + (c.size_bytes || 0), 0);
      const countText = `${fmtCount(clips.length)} ${clips.length === 1 ? 'clip' : 'clips'}`;
      $('clips-summary').textContent = clips.length > 0
        ? `${countText} · ${fmtBytes(totalBytes)}`
        : countText;
    }
  }

  function updateCardSelectionUI() {
    for (const card of cardMap.values()) {
      const isSel = selectedClipPaths.has(card.dataset.path);
      card.classList.toggle('is-selected', isSel);
      const chk = cardRefs.get(card)?.check;
      if (chk) chk.setAttribute('aria-pressed', String(isSel));
    }
    updateClipsToolbar();
  }

  function clearClipSelection() {
    selectMode = false;
    selectedClipPaths.clear();
    selectionAnchorIdx = null;
    updateCardSelectionUI();
  }

  function toggleClipSelection(idx, { range = false, additive = false, forceToggle = false } = {}) {
    const clip = clips[idx];
    if (!clip) return;
    if (range && selectionAnchorIdx != null && selectionAnchorIdx < clips.length) {
      const lo = Math.min(selectionAnchorIdx, idx);
      const hi = Math.max(selectionAnchorIdx, idx);
      if (!additive) selectedClipPaths.clear();
      for (let i = lo; i <= hi; i++) {
        selectedClipPaths.add(clips[i].path);
      }
    } else if (forceToggle || additive || selectMode) {
      if (selectedClipPaths.has(clip.path)) {
        selectedClipPaths.delete(clip.path);
      } else {
        selectedClipPaths.add(clip.path);
      }
      selectionAnchorIdx = idx;
    } else {
      if (selectedClipPaths.size === 1 && selectedClipPaths.has(clip.path)) {
        selectedClipPaths.clear();
        selectionAnchorIdx = null;
      } else {
        selectedClipPaths.clear();
        selectedClipPaths.add(clip.path);
        selectionAnchorIdx = idx;
      }
    }
    updateCardSelectionUI();
  }

  function applyPreviewToCard(card, clip, preview) {
    if (!card || !preview) return;
    const refs = cardRefs.get(card);
    if (!refs) return;
    if (preview.thumb_url) {
      let img = refs.img;
      if (!img) {
        img = document.createElement('img');
        img.className = 'clip-thumb';
        img.decoding = 'async';
        img.loading = 'lazy';
        img.draggable = false;
        img.addEventListener('error', () => {
          img.remove();
          if (refs.img === img) refs.img = null;
          refs.placeholder.textContent = 'No preview';
          refs.placeholder.classList.remove('is-hidden');
        });
        refs.thumbWrap.insertBefore(img, refs.thumbWrap.firstChild);
        refs.img = img;
      }
      img.alt = clip.display_name;
      if (img.src !== preview.thumb_url) {
        img.src = preview.thumb_url;
      }
      refs.placeholder.classList.add('is-hidden');
    } else {
      refs.placeholder.textContent = 'No preview';
      refs.placeholder.classList.remove('is-hidden');
    }
    const durStr = fmtDurationMs(preview.duration_ms ?? clip.duration_ms);
    if (durStr) {
      refs.duration.textContent = durStr;
      refs.duration.classList.remove('is-hidden');
    } else {
      refs.duration.classList.add('is-hidden');
    }
    refs.meta.textContent = clipMetaText(clip);
  }

  function pumpPreviewQueue() {
    while (activePreviewWorkers < 4 && previewQueue.length > 0) {
      const item = previewQueue.shift();
      if (!item) break;
      const { clip, key } = item;
      if (clipPreviewCache.has(key)) {
        queuedPreviewKeys.delete(key);
        continue;
      }
      activePreviewWorkers++;
      invoke('get_clip_preview', { path: clip.path })
        .then((res) => {
          clipPreviewCache.set(key, res);
          prunePreviewCache();
          const card = cardMap.get(clip.path);
          if (card && card.dataset.key === key) {
            applyPreviewToCard(card, clip, res);
          }
        })
        .catch(() => {})
        .finally(() => {
          queuedPreviewKeys.delete(key);
          activePreviewWorkers--;
          pumpPreviewQueue();
        });
    }
  }

  function enqueuePreview(clip, urgent = false) {
    const key = clipCacheKey(clip);
    if (clipPreviewCache.has(key) || queuedPreviewKeys.has(key)) return;
    queuedPreviewKeys.add(key);
    const job = { clip, key };
    if (urgent) previewQueue.unshift(job);
    else previewQueue.push(job);
    pumpPreviewQueue();
  }

  function ensureThumbObserver() {
    if (thumbObserver) {
      thumbObserver.disconnect();
    }
    const root = document.querySelector('.content-scroll');
    thumbObserver = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (!entry.isIntersecting) continue;
          const card = entry.target;
          const idx = Number(card.dataset.index);
          const clip = clips[idx];
          if (clip) enqueuePreview(clip, true);
          thumbObserver.unobserve(card);
        }
      },
      { root, rootMargin: '180px 0px' },
    );
  }

  function makeSvgIcon(pathD, viewBox = '0 0 16 16') {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', viewBox);
    svg.setAttribute('fill', 'none');
    svg.setAttribute('aria-hidden', 'true');
    pathD.forEach((d) => {
      const p = document.createElementNS('http://www.w3.org/2000/svg', 'path');
      Object.entries(d).forEach(([k, v]) => p.setAttribute(k, v));
      svg.appendChild(p);
    });
    return svg;
  }

  const SHARE_ICONS = {
    share: [
      { d: 'M10.5 3.5H13V6M12.5 3.5L7.5 8.5M6.5 4.5H3.5C2.95 4.5 2.5 4.95 2.5 5.5V12.5C2.5 13.05 2.95 13.5 3.5 13.5H10.5C11.05 13.5 11.5 13.05 11.5 12.5V9.5', stroke: 'currentColor', 'stroke-width': '1.35', 'stroke-linecap': 'round', 'stroke-linejoin': 'round' },
    ],
    link: [
      { d: 'M6.6 9.4a2.4 2.4 0 0 0 3.4 0l2-2a2.4 2.4 0 0 0-3.4-3.4l-1 1', stroke: 'currentColor', 'stroke-width': '1.35', 'stroke-linecap': 'round', 'stroke-linejoin': 'round' },
      { d: 'M9.4 6.6a2.4 2.4 0 0 0-3.4 0l-2 2a2.4 2.4 0 0 0 3.4 3.4l1-1', stroke: 'currentColor', 'stroke-width': '1.35', 'stroke-linecap': 'round', 'stroke-linejoin': 'round' },
    ],
    file: [
      { d: 'M5.5 5.5H13.5V13.5H5.5V5.5Z', stroke: 'currentColor', 'stroke-width': '1.35', 'stroke-linejoin': 'round' },
      { d: 'M10.5 5.5V2.5H2.5V10.5H5.5', stroke: 'currentColor', 'stroke-width': '1.35', 'stroke-linejoin': 'round' },
    ],
  };

  function createClipCard(clip) {
    const clipForCard = () => currentClipForCard(card);

    const card = document.createElement('article');
    card.className = 'glass-card clip-card';
    card.dataset.path = clip.path;

    // Thumbnail
    const thumbWrap = document.createElement('div');
    thumbWrap.className = 'clip-thumb-wrap';
    thumbWrap.title = clip.file_name;

    const placeholder = document.createElement('div');
    placeholder.className = 'clip-thumb-placeholder';
    placeholder.textContent = 'Loading…';
    thumbWrap.appendChild(placeholder);

    const checkBtn = document.createElement('button');
    checkBtn.type = 'button';
    checkBtn.className = 'clip-check';
    checkBtn.title = 'Select clip';
    checkBtn.setAttribute('aria-label', `Select ${clip.display_name}`);
    checkBtn.setAttribute('aria-pressed', 'false');
    checkBtn.appendChild(
      makeSvgIcon([
        { d: 'M3 8.2L6.4 11.5L13 4.8', stroke: 'currentColor', 'stroke-width': '2', 'stroke-linecap': 'round', 'stroke-linejoin': 'round' },
      ]),
    );
    checkBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      toggleClipSelection(Number(card.dataset.index), { range: e.shiftKey, additive: true, forceToggle: true });
    });
    thumbWrap.appendChild(checkBtn);

    const durBadge = document.createElement('span');
    durBadge.className = 'clip-duration is-hidden';
    thumbWrap.appendChild(durBadge);

    thumbWrap.addEventListener('click', (e) => {
      toggleClipSelection(Number(card.dataset.index), {
        range: e.shiftKey,
        additive: e.ctrlKey || e.metaKey,
      });
    });

    card.addEventListener('contextmenu', (e) => {
      e.preventDefault();
      const target = clipForCard();
      if (!target) return;
      if (!selectedClipPaths.has(target.path)) {
        selectedClipPaths.clear();
        selectedClipPaths.add(target.path);
        selectionAnchorIdx = Number(card.dataset.index);
        updateCardSelectionUI();
      }
      showContextMenu(e.clientX, e.clientY, target);
    });

    // Body
    const body = document.createElement('div');
    body.className = 'clip-body';

    const title = document.createElement('div');
    title.className = 'clip-title';
    title.textContent = clip.display_name;
    title.title = clip.file_name;

    const meta = document.createElement('div');
    meta.className = 'clip-meta';
    meta.textContent = clipMetaText(clip);

    const actions = document.createElement('div');
    actions.className = 'clip-actions';

    const shareBtn = document.createElement('button');
    shareBtn.type = 'button';
    shareBtn.className = 'btn btn-secondary btn-xs clip-share-btn is-share';
    // The icon node is swapped in place by applyShareButtonUI, never accumulated.
    const shareIcon = makeSvgIcon(SHARE_ICONS.share);
    shareBtn.appendChild(shareIcon);
    const shareSpan = document.createElement('span');
    shareBtn.appendChild(shareSpan);
    shareBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      const target = clipForCard();
      if (!target) return;
      if (!isShareConfigured()) copyClipFile(target);
      else shareClip(target);
    });

    const iconActions = document.createElement('div');
    iconActions.className = 'clip-icon-actions';

    const revealBtn = document.createElement('button');
    revealBtn.type = 'button';
    revealBtn.className = 'clip-icon-btn';
    revealBtn.title = 'View in File Explorer';
    revealBtn.setAttribute('aria-label', 'View in File Explorer');
    revealBtn.appendChild(
      makeSvgIcon([
        { d: 'M1.5 3.5H6L7.5 5.2H14.5V12.5H1.5V3.5Z', stroke: 'currentColor', 'stroke-width': '1.35', 'stroke-linejoin': 'round' },
      ]),
    );
    revealBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      const target = clipForCard();
      if (target) revealClipInExplorer(target.path);
    });

    const renameBtn = document.createElement('button');
    renameBtn.type = 'button';
    renameBtn.className = 'clip-icon-btn';
    renameBtn.title = 'Rename';
    renameBtn.setAttribute('aria-label', 'Rename');
    renameBtn.appendChild(
      makeSvgIcon([
        { d: 'M11.4 2.4L13.6 4.6L5.2 13H3V10.8L11.4 2.4Z', stroke: 'currentColor', 'stroke-width': '1.35', 'stroke-linejoin': 'round' },
      ]),
    );
    renameBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      const target = clipForCard();
      if (target) openRenameModal(target);
    });

    const deleteBtn = document.createElement('button');
    deleteBtn.type = 'button';
    deleteBtn.className = 'clip-icon-btn is-danger';
    deleteBtn.title = 'Delete';
    deleteBtn.setAttribute('aria-label', 'Delete');
    deleteBtn.appendChild(
      makeSvgIcon([
        { d: 'M3 4.5H13M6 4.5V3H10V4.5M5 4.5V13H11V4.5', stroke: 'currentColor', 'stroke-width': '1.35', 'stroke-linecap': 'round', 'stroke-linejoin': 'round' },
      ]),
    );
    deleteBtn.addEventListener('click', (e) => {
      e.stopPropagation();
      const target = clipForCard();
      if (target) openDeleteModal([target.path]);
    });

    iconActions.appendChild(revealBtn);
    iconActions.appendChild(renameBtn);
    iconActions.appendChild(deleteBtn);

    actions.appendChild(shareBtn);
    actions.appendChild(iconActions);

    body.appendChild(title);
    body.appendChild(meta);
    body.appendChild(actions);

    card.appendChild(thumbWrap);
    card.appendChild(body);

    cardRefs.set(card, {
      thumbWrap,
      placeholder,
      img: null,
      duration: durBadge,
      title,
      meta,
      check: checkBtn,
      share: shareBtn,
      shareLabel: shareSpan,
      shareIcon,
      shareIconKey: SHARE_ICONS.share,
    });
    applyShareButtonUI(cardRefs.get(card), clip);
    return card;
  }

  function updateClipCard(card, clip, idx) {
    const key = clipCacheKey(clip);
    const keyChanged = card.dataset.key !== key;
    card.dataset.path = clip.path;
    card.dataset.index = String(idx);

    const refs = cardRefs.get(card);
    const isSelected = selectedClipPaths.has(clip.path);
    card.classList.toggle('is-selected', isSelected);
    if (refs) {
      refs.thumbWrap.title = clip.file_name;
      refs.title.textContent = clip.display_name;
      refs.title.title = clip.file_name;
      refs.check.setAttribute('aria-label', `Select ${clip.display_name}`);
      refs.check.setAttribute('aria-pressed', String(isSelected));
    }

    if (keyChanged) {
      card.dataset.key = key;
      if (refs) {
        if (refs.img) {
          refs.img.remove();
          refs.img = null;
        }
        refs.placeholder.textContent = 'Loading…';
        refs.placeholder.classList.remove('is-hidden');
        refs.duration.classList.add('is-hidden');
      }
    }

    const cached = getCachedPreview(clip);
    if (cached) {
      applyPreviewToCard(card, clip, cached);
    } else if (refs) {
      refs.meta.textContent = clipMetaText(clip);
      const dur = fmtDurationMs(clip.duration_ms);
      if (dur) {
        refs.duration.textContent = dur;
        refs.duration.classList.remove('is-hidden');
      }
      if (activeTab === 'clips' && thumbObserver) {
        thumbObserver.observe(card);
      }
    }

    applyShareButtonUI(refs, clip);
  }

  function resetClipsView() {
    clipPreviewCache.clear();
    previewQueue = [];
    queuedPreviewKeys.clear();
    cardMap.clear();
    clips = [];
    selectMode = false;
    selectedClipPaths.clear();
    selectionAnchorIdx = null;
    clipsLoadedOnce = false;
    clipsLoadedDir = null;
    const grid = $('clips-grid');
    if (grid) grid.replaceChildren();
    updateClipsToolbar();
  }

  function renderClipsGallery() {
    const grid = $('clips-grid');
    const empty = $('clips-empty');
    const scrollEl = document.querySelector('.content-scroll');
    const prevScroll = scrollEl ? scrollEl.scrollTop : 0;

    const validPaths = new Set(clips.map((c) => c.path));
    for (const p of Array.from(selectedClipPaths)) {
      if (!validPaths.has(p)) selectedClipPaths.delete(p);
    }
    // A deleted clip must not stay "uploading" forever and mark a re-recorded file as busy.
    for (const p of Array.from(sharingPaths)) {
      if (!validPaths.has(p)) sharingPaths.delete(p);
    }
    if (clips.length === 0) {
      selectMode = false;
      selectionAnchorIdx = null;
    }

    const hint = $('clips-empty-hint');
    if (hint) {
      const key = recState && recState.hotkey_label ? recState.hotkey_label : 'F9';
      hint.textContent = `Press ${key} while playing to save a clip.`;
    }

    empty.classList.toggle('is-hidden', clips.length > 0);
    grid.classList.toggle('is-hidden', clips.length === 0);

    ensureThumbObserver();

    // Reconcile in place: existing cards keep their DOM and loaded thumbnails; new cards go into
    // a fragment and everything is ordered with a single insertion (no `grid.innerHTML = ''`).
    const frag = document.createDocumentFragment();
    clips.forEach((clip, idx) => {
      let card = cardMap.get(clip.path);
      if (!card) {
        card = createClipCard(clip);
        cardMap.set(clip.path, card);
      }
      updateClipCard(card, clip, idx);
      frag.appendChild(card);
    });
    for (const [path, card] of Array.from(cardMap)) {
      if (!validPaths.has(path)) {
        card.remove();
        cardRefs.delete(card);
        cardMap.delete(path);
        dropPreviewForPath(path);
      }
    }
    grid.appendChild(frag);

    updateClipsToolbar();
    if (scrollEl && prevScroll > 0) {
      scrollEl.scrollTop = prevScroll;
    }
    if (activeTab === 'clips') {
      syncWindowHeight(true);
      verifyClipsWindowHeight();
    }
  }

  function observePendingPreviews() {
    ensureThumbObserver();
    for (const card of cardMap.values()) {
      const clip = currentClipForCard(card);
      if (!clip) continue;
      const cached = getCachedPreview(clip);
      if (cached) {
        applyPreviewToCard(card, clip, cached);
      } else if (thumbObserver) {
        thumbObserver.observe(card);
      }
    }
  }

  function clipsSignature(list) {
    return list
      .map((c) => `${c.path}|${c.size_bytes}|${c.modified_ms}|${c.uploaded_url || ''}|${c.uploading ? '1' : '0'}`)
      .join('\n');
  }

  async function refreshClips({ silent = false } = {}) {
    const token = ++clipsRefreshToken;
    try {
      const list = await invoke('list_clips');
      if (token !== clipsRefreshToken) return;
      const next = Array.isArray(list) ? list : [];
      const curDir = cfg ? cfg.output_dir : null;
      const unchanged = clipsLoadedOnce && clipsLoadedDir === curDir && clipsSignature(clips) === clipsSignature(next);
      clipsLoadedOnce = true;
      clipsLoadedDir = curDir;
      if (!unchanged) {
        clips = next;
        renderClipsGallery();
      }
    } catch (e) {
      if (!silent) {
        showToast('error', "Couldn't load clips", String(e));
      }
    }
  }

  async function revealClipInExplorer(path) {
    try {
      await invoke('reveal_clip', { path });
    } catch (e) {
      showToast('error', "Couldn't open File Explorer", String(e));
    }
  }

  function isShareConfigured() {
    return Boolean(cfg && cfg.share_url?.trim() && cfg.has_share_key);
  }

  function isClipUploading(clip) {
    return sharingPaths.has(clip?.path) || Boolean(clip?.uploading);
  }

  // Resolves what the card share button must offer: upload+copy, copy the existing link, or copy
  // the file itself when Cloudflare R2 is not set up.
  function shareButtonSpec(clip) {
    if (isClipUploading(clip)) {
      return { label: 'Uploading…', title: 'Uploading…', aria: 'Uploading…', icon: null, busy: true };
    }
    if (!isShareConfigured()) {
      return {
        label: 'Copy file',
        title: 'Copy file to clipboard',
        aria: 'Copy file to clipboard',
        icon: SHARE_ICONS.file,
        busy: false,
      };
    }
    if (clip?.uploaded_url) {
      return {
        label: 'Copy link',
        title: 'Copy link to clipboard',
        aria: 'Copy link to clipboard',
        icon: SHARE_ICONS.link,
        busy: false,
      };
    }
    return {
      label: 'Share link',
      title: 'Share & copy link',
      aria: 'Share and copy link',
      icon: SHARE_ICONS.share,
      busy: false,
    };
  }

  function applyShareButtonUI(refs, clip) {
    const btn = refs?.share;
    if (!btn) return;
    const spec = shareButtonSpec(clip);
    btn.disabled = spec.busy;
    btn.classList.toggle('is-busy', spec.busy);
    btn.title = spec.title;
    btn.setAttribute('aria-label', spec.aria);
    if (refs.shareLabel && refs.shareLabel.textContent !== spec.label) {
      refs.shareLabel.textContent = spec.label;
    }
    // Busy keeps the last icon: the state is temporary and re-rendering would only churn nodes.
    if (spec.icon && refs.shareIconKey !== spec.icon) {
      const icon = makeSvgIcon(spec.icon);
      if (refs.shareIcon && refs.shareIcon.parentNode === btn) {
        refs.shareIcon.replaceWith(icon);
      } else {
        btn.insertBefore(icon, btn.firstChild);
      }
      refs.shareIcon = icon;
      refs.shareIconKey = spec.icon;
    }
  }

  function updateCardShareUI(card, clip) {
    if (!card) return;
    applyShareButtonUI(cardRefs.get(card), clip);
  }

  function updateCardShareByPath(path) {
    const card = cardMap.get(path);
    if (!card) return;
    const clip = clips.find((c) => c.path === path) || null;
    updateCardShareUI(card, clip);
  }

  function refreshAllCardShareUI() {
    for (const [path, card] of cardMap) {
      updateCardShareUI(card, clips.find((c) => c.path === path) || null);
    }
  }

  // Mirrors backend upload state onto the clip object, the local busy set and the card button.
  function setClipShareState(path, { uploading, uploadedUrl } = {}) {
    if (uploading) sharingPaths.add(path);
    else if (uploading === false) sharingPaths.delete(path);

    const clip = clips.find((c) => c.path === path);
    if (clip) {
      if (uploading !== undefined) clip.uploading = Boolean(uploading);
      if (uploadedUrl !== undefined) clip.uploaded_url = uploadedUrl || null;
    }
    updateCardShareByPath(path);
  }

  async function copyClipFile(clip) {
    if (!clip) return;
    try {
      await invoke('copy_clip_file', { path: clip.path });
      showToast('info', 'File copied', clip.display_name || clip.file_name || '');
    } catch (e) {
      showToast('error', "Couldn't copy file", String(e));
    }
  }

  async function shareClip(clip) {
    if (!clip) return;
    if (!isShareConfigured()) {
      await copyClipFile(clip);
      return;
    }
    if (isClipUploading(clip)) return;

    // Already-uploaded clips copy immediately without flashing an "Uploading…" state.
    const needsUpload = !clip.uploaded_url;
    if (needsUpload) {
      setClipShareState(clip.path, { uploading: true });
    }
    try {
      const res = await invoke('share_clip', { path: clip.path });
      let copied = Boolean(res.copied);
      if (!copied && res.url && navigator.clipboard?.writeText) {
        try {
          await navigator.clipboard.writeText(res.url);
          copied = true;
        } catch (_) {}
      }
      showToast('info', copied ? 'Link copied' : 'Clip uploaded', res.url || '');
      if (res.url) setClipShareState(clip.path, { uploadedUrl: res.url });
    } catch (e) {
      showToast('error', "Couldn't share clip", String(e));
    } finally {
      if (needsUpload) {
        setClipShareState(clip.path, { uploading: false });
      }
    }
  }

  function hideContextMenu() {
    const menu = $('clip-context-menu');
    if (menu) menu.classList.add('is-hidden');
  }

  function showContextMenu(x, y, clip) {
    const menu = $('clip-context-menu');
    menu.innerHTML = '';

    const addItem = (label, onClick, danger = false) => {
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = `context-item${danger ? ' is-danger' : ''}`;
      btn.textContent = label;
      btn.addEventListener('click', () => {
        hideContextMenu();
        onClick();
      });
      menu.appendChild(btn);
    };
    const addSep = () => {
      const sep = document.createElement('div');
      sep.className = 'context-sep';
      menu.appendChild(sep);
    };

    const selectedList = clips.filter((c) => selectedClipPaths.has(c.path)).map((c) => c.path);
    if (selectedList.length > 1) {
      addItem(`Delete ${fmtCount(selectedList.length)} selected`, () => openDeleteModal(selectedList), true);
      addItem('Clear selection', clearClipSelection);
    } else {
      if (!isShareConfigured()) {
        addItem('Copy file', () => copyClipFile(clip));
      } else if (clip.uploaded_url) {
        addItem('Copy link', () => shareClip(clip));
      } else {
        addItem('Share & copy link', () => shareClip(clip));
      }
      addItem('View in File Explorer', () => revealClipInExplorer(clip.path));
      addItem('Rename', () => openRenameModal(clip));
      addSep();
      addItem('Delete', () => openDeleteModal([clip.path]), true);
    }

    menu.classList.remove('is-hidden');
    const rect = menu.getBoundingClientRect();
    const left = Math.min(x, window.innerWidth - rect.width - 8);
    const top = Math.min(y, window.innerHeight - rect.height - 8);
    menu.style.left = `${Math.max(8, left)}px`;
    menu.style.top = `${Math.max(8, top)}px`;
  }

  function openRenameModal(clip) {
    renameTargetClip = clip;
    $('rename-clip-subtitle').textContent = clip.file_name;
    const input = $('inp-rename-clip');
    input.value = clip.stem || '';
    $('rename-clip-ext').textContent = clip.ext ? `.${clip.ext}` : '';
    $('rename-clip-error').textContent = '';
    $('rename-clip-error').classList.add('is-hidden');
    $('btn-rename-save').disabled = false;
    $('btn-rename-save').textContent = 'Save';
    $('modal-rename-clip').classList.remove('is-hidden');
    requestAnimationFrame(() => {
      input.focus();
      input.select();
    });
  }

  function closeRenameModal() {
    renameTargetClip = null;
    $('modal-rename-clip').classList.add('is-hidden');
  }

  async function submitRenameModal() {
    if (!renameTargetClip) return;
    const oldClip = renameTargetClip;
    const newName = $('inp-rename-clip').value;
    const saveBtn = $('btn-rename-save');
    const errEl = $('rename-clip-error');
    saveBtn.disabled = true;
    saveBtn.textContent = 'Saving…';
    errEl.classList.add('is-hidden');
    try {
      const updated = await invoke('rename_clip', { path: oldClip.path, newName });
      const oldKey = clipCacheKey(oldClip);
      const cached = clipPreviewCache.get(oldKey);
      dropPreviewForPath(oldClip.path);
      if (cached) {
        clipPreviewCache.set(clipCacheKey(updated), { ...cached, path: updated.path });
        prunePreviewCache();
      }
      if (selectedClipPaths.has(oldClip.path)) {
        selectedClipPaths.delete(oldClip.path);
        selectedClipPaths.add(updated.path);
      }
      closeRenameModal();
      await refreshClips({ silent: true });
      recState = await invoke('recorder_state');
      renderState();
    } catch (e) {
      errEl.textContent = String(e);
      errEl.classList.remove('is-hidden');
      saveBtn.disabled = false;
      saveBtn.textContent = 'Save';
    }
  }

  function openDeleteModal(paths) {
    if (!paths || paths.length === 0) return;
    deleteTargetPaths = paths.slice();
    const n = deleteTargetPaths.length;
    const targets = clips.filter((c) => deleteTargetPaths.includes(c.path));
    const totalBytes = targets.reduce((sum, c) => sum + (c.size_bytes || 0), 0);

    $('modal-delete-title').textContent = n === 1
      ? 'Delete this clip?'
      : `Delete ${fmtCount(n)} clips?`;
    $('modal-delete-subtitle').textContent = n === 1 && targets[0]
      ? `${targets[0].display_name} · ${fmtBytes(targets[0].size_bytes)}`
      : `${fmtCount(n)} clips · ${fmtBytes(totalBytes)} total`;
    $('modal-delete-body').textContent = n === 1
      ? 'It goes to the Windows Recycle Bin. If you change your mind, you can restore it from there.'
      : 'They go to the Windows Recycle Bin. If you change your mind, you can restore them from there.';

    const confirmBtn = $('btn-delete-confirm');
    confirmBtn.disabled = false;
    confirmBtn.textContent = 'Delete';
    $('modal-delete-clips').classList.remove('is-hidden');
  }

  function closeDeleteModal() {
    deleteTargetPaths = [];
    $('modal-delete-clips').classList.add('is-hidden');
  }

  async function confirmDeleteModal() {
    if (!deleteTargetPaths.length) return;
    const paths = deleteTargetPaths.slice();
    const confirmBtn = $('btn-delete-confirm');
    confirmBtn.disabled = true;
    confirmBtn.textContent = 'Deleting…';
    try {
      const res = await invoke('delete_clips', { paths });
      const deletedSet = new Set(res.deleted || []);
      deletedSet.forEach((p) => {
        selectedClipPaths.delete(p);
        dropPreviewForPath(p);
      });
      if (selectedClipPaths.size === 0) {
        selectMode = false;
        selectionAnchorIdx = null;
      }
      closeDeleteModal();
      await refreshClips({ silent: true });
      recState = await invoke('recorder_state');
      renderState();
      if (res.failed && res.failed.length > 0) {
        const first = res.failed[0];
        showToast('error', `Couldn't delete ${res.failed.length} clip(s)`, `${first.name}: ${first.error}`);
      } else if (deletedSet.size > 0) {
        showToast(
          'info',
          deletedSet.size === 1 ? 'Clip moved to Recycle Bin' : `${fmtCount(deletedSet.size)} clips moved to Recycle Bin`,
          '',
        );
      }
    } catch (e) {
      closeDeleteModal();
      showToast('error', "Couldn't delete clips", String(e));
    }
  }

  async function cancelCapture() {
    capturingHotkey = false;
    renderHotkey();
    try {
      await invoke('cancel_hotkey_capture');
    } catch (_) {}
  }

  function renderHotkey() {
    const btn = $('btn-hotkey');
    const hint = $('hotkey-hint');
    if (capturingHotkey) {
      btn.textContent = 'Press…';
      btn.classList.add('is-waiting');
      hint.classList.remove('is-hidden');
    } else {
      btn.textContent = (recState && recState.hotkey_label) ? recState.hotkey_label : 'F9';
      btn.classList.remove('is-waiting');
      hint.classList.add('is-hidden');
    }
  }

  function renderConfig() {
    if (!cfg) return;

    renderHotkey();

    // Clip length
    const secSlider = $('slider-seconds');
    secSlider.value = String(cfg.seconds);
    updateRangeFill(secSlider);
    $('val-seconds').textContent = fmtSecs(cfg.seconds);

    // Bitrate
    const brSlider = $('slider-bitrate');
    brSlider.value = String(cfg.bitrate_mbps);
    updateRangeFill(brSlider);
    $('val-bitrate').textContent = `${cfg.bitrate_mbps} Mbps`;

    // Frame rate
    document.querySelectorAll('#seg-fps .seg-btn').forEach((btn) => {
      btn.classList.toggle('is-active', Number(btn.dataset.fps) === cfg.fps);
    });

    // Resolution
    const maxH = Number(cfg.max_height || 0);
    document.querySelectorAll('#seg-res .seg-btn').forEach((btn) => {
      btn.classList.toggle('is-active', Number(btn.dataset.height) === maxH);
    });

    // Video codec
    const codec = cfg.codec || 'h264';
    document.querySelectorAll('#seg-codec .seg-btn').forEach((btn) => {
      btn.classList.toggle('is-active', btn.dataset.codec === codec);
    });

    // Capture mode
    const captureMode = cfg.capture_mode || 'games';
    document.querySelectorAll('#seg-capture .seg-btn').forEach((btn) => {
      btn.classList.toggle('is-active', btn.dataset.capture === captureMode);
    });

    // Folder
    const outDir = cfg.output_dir || '';
    $('txt-output-dir').textContent = outDir;
    $('txt-output-dir').title = outDir;

    // Audio channels
    $('chk-game-audio').checked = Boolean(cfg.game_audio);
    $('slider-game-vol').disabled = !cfg.game_audio;
    $('slider-game-vol').value = String(cfg.game_volume);
    updateRangeFill($('slider-game-vol'));
    $('val-game-vol').textContent = `${cfg.game_volume} %`;
    $('wrap-game-vol').classList.toggle('is-disabled', !cfg.game_audio);

    $('chk-desktop-audio').checked = Boolean(cfg.desktop_audio);
    $('slider-desktop-vol').disabled = !cfg.desktop_audio;
    $('slider-desktop-vol').value = String(cfg.desktop_volume);
    updateRangeFill($('slider-desktop-vol'));
    $('val-desktop-vol').textContent = `${cfg.desktop_volume} %`;
    $('wrap-desktop-vol').classList.toggle('is-disabled', !cfg.desktop_audio);

    $('chk-mic-audio').checked = Boolean(cfg.mic_audio);
    $('slider-mic-vol').disabled = !cfg.mic_audio;
    $('slider-mic-vol').value = String(cfg.mic_volume);
    updateRangeFill($('slider-mic-vol'));
    $('val-mic-vol').textContent = `${cfg.mic_volume} %`;
    $('wrap-mic-vol').classList.toggle('is-disabled', !cfg.mic_audio);

    // Input device
    $('row-mic-device').classList.toggle('is-hidden', !cfg.mic_audio);
    renderMicList();

    // Preferences
    const soundOn = Boolean(cfg.sound_on_save);
    const soundVol = Number(cfg.sound_volume ?? 100);
    $('chk-sound-on-save').checked = soundOn;
    $('slider-sound-vol').disabled = !soundOn;
    $('slider-sound-vol').value = String(soundVol);
    updateRangeFill($('slider-sound-vol'));
    $('val-sound-vol').textContent = `${soundVol} %`;
    $('wrap-sound-vol').classList.toggle('is-disabled', !soundOn);

    // Clip sharing
    const urlInp = $('inp-share-url');
    if (urlInp && document.activeElement !== urlInp) {
      urlInp.value = cfg.share_url || '';
    }
    // The upload key never comes back from the backend: only its presence is known.
    const keyInp = $('inp-share-key');
    if (keyInp) {
      if (document.activeElement !== keyInp) {
        keyInp.value = '';
      }
      keyInp.placeholder = cfg.has_share_key
        ? 'Configured (enter a new key to replace)'
        : 'Secret upload key';
    }
    const clearKeyBtn = $('btn-clear-share-key');
    if (clearKeyBtn) clearKeyBtn.disabled = !cfg.has_share_key;

    // Turning R2 on or off changes what every card button does; changing the share URL invalidates
    // the uploads recorded for it, so the gallery is re-listed.
    const shareKey = `${(cfg.share_url || '').trim()}|${cfg.has_share_key ? '1' : '0'}`;
    if (renderedShareConfig !== null && renderedShareConfig !== shareKey) {
      refreshAllCardShareUI();
      if (clipsLoadedOnce) {
        if (activeTab === 'clips') refreshClips({ silent: true });
        else clipsLoadedOnce = false;
      }
    }
    renderedShareConfig = shareKey;

    renderEstimatedSize();

    syncWindowHeight(true);
  }

  function estimateClipSizeText(c) {
    if (!c) return '—';
    const hasAudio = Boolean(c.game_audio || c.desktop_audio || c.mic_audio);
    const audioMbps = hasAudio ? 0.192 : 0;
    const totalMb = ((c.bitrate_mbps + audioMbps) * c.seconds) / 8;
    if (totalMb >= 1024) {
      return `~${(totalMb / 1024).toFixed(2)} GB`;
    }
    if (totalMb >= 100) {
      return `~${Math.round(totalMb)} MB`;
    }
    return `~${totalMb.toFixed(1)} MB`;
  }

  function renderEstimatedSize() {
    const el = $('val-est-size');
    if (el) el.textContent = estimateClipSizeText(cfg);
  }

  function renderMicList() {
    if (!cfg) return;
    const sel = $('sel-mic-device');
    sel.innerHTML = '';

    const defOpt = document.createElement('option');
    defOpt.value = '';
    defOpt.textContent = 'Windows default';
    sel.appendChild(defOpt);

    let found = cfg.mic_device === '';
    for (const m of mics) {
      const opt = document.createElement('option');
      opt.value = m.id;
      opt.textContent = m.name;
      if (m.id === cfg.mic_device) found = true;
      sel.appendChild(opt);
    }
    if (!found && cfg.mic_device) {
      const missingOpt = document.createElement('option');
      missingOpt.value = cfg.mic_device;
      missingOpt.textContent = 'Microphone disconnected';
      sel.appendChild(missingOpt);
    }
    sel.value = cfg.mic_device || '';
    const selectedText = sel.options[sel.selectedIndex]?.textContent || 'Windows default';
    sel.title = selectedText;
  }

  function renderState() {
    if (!recState) return;

    renderHotkey();

    $('chk-autostart').checked = Boolean(recState.autostart);
    $('txt-app-dir').textContent = recState.app_dir;
    $('txt-app-dir').title = recState.app_dir;

    const dot = $('status-dot');
    const txt = $('status-text');
    const toggleWrap = $('status-toggle-wrap');
    const statusChk = $('chk-status-enabled');
    const startBtn = $('btn-start-recorder');
    const hint = $('status-hotkey-hint');

    dot.classList.remove('is-muted', 'is-error');
    txt.classList.remove('is-muted');

    if (!recState.running) {
      dot.classList.add('is-muted');
      txt.classList.add('is-muted');
      txt.textContent = 'Recorder closed';
      toggleWrap.classList.add('is-hidden');
      startBtn.classList.remove('is-hidden');
      hint.textContent = '';
    } else if (recState.error) {
      dot.classList.add('is-error');
      txt.textContent = 'Error';
      toggleWrap.classList.remove('is-hidden');
      statusChk.checked = Boolean(recState.enabled);
      startBtn.classList.add('is-hidden');
      hint.textContent = `· ${recState.error}`;
    } else if (recState.idle_waiting) {
      txt.textContent = 'Waiting for game';
      toggleWrap.classList.remove('is-hidden');
      statusChk.checked = true;
      startBtn.classList.add('is-hidden');
      hint.textContent = `· 0% GPU idle · ${recState.hotkey_label} saves ${fmtSecs(recState.seconds)}`;
    } else if (recState.enabled) {
      txt.textContent = 'Recording';
      toggleWrap.classList.remove('is-hidden');
      statusChk.checked = true;
      startBtn.classList.add('is-hidden');
      hint.textContent = `· ${recState.hotkey_label} saves the last ${fmtSecs(recState.seconds)}`;
    } else {
      dot.classList.add('is-muted');
      txt.classList.add('is-muted');
      txt.textContent = 'Paused';
      toggleWrap.classList.remove('is-hidden');
      statusChk.checked = false;
      startBtn.classList.add('is-hidden');
      hint.textContent = '';
    }

    const clips = recState.clips_count || 0;
    const clipLabel = `${fmtCount(clips)} ${clips === 1 ? 'clip' : 'clips'}`;
    $('status-meta').textContent = recState.running
      ? `~${recState.ram_mb} MB RAM · ${clipLabel}`
      : clipLabel;
  }

  async function commitConfigNow() {
    if (saveTimer) {
      clearTimeout(saveTimer);
      saveTimer = null;
    }
    if (!cfg) return;
    try {
      cfg = await invoke('set_config', { cfg });
    } catch (e) {
      showToast('error', 'Save failed', String(e));
    }
  }

  function scheduleConfigCommit() {
    if (saveTimer) clearTimeout(saveTimer);
    saveTimer = setTimeout(() => {
      saveTimer = null;
      commitConfigNow();
    }, 250);
  }

  function bindRange(id, getter, setter, onLabel) {
    const el = $(id);
    el.addEventListener('pointerdown', () => { draggingSlider = true; });
    el.addEventListener('pointerup', () => { draggingSlider = false; });
    el.addEventListener('input', () => {
      if (!cfg) return;
      const v = Number(el.value);
      setter(v);
      updateRangeFill(el);
      onLabel(v);
      scheduleConfigCommit();
    });
    el.addEventListener('change', () => {
      draggingSlider = false;
      if (!cfg) return;
      setter(Number(el.value));
      commitConfigNow();
    });
  }

  async function syncMaximizedState() {
    try {
      const max = Boolean(await appWindow.isMaximized());
      const shell = document.querySelector('.app-shell');
      if (shell) shell.classList.toggle('is-maximized', max);
      if (wasMaximized && !max) {
        syncWindowHeight(false);
      }
      wasMaximized = max;
    } catch (_) {}
  }

  function bindEvents() {
    // Window controls (titlebar drag handled by data-tauri-drag-region)
    $('win-min').addEventListener('click', () => appWindow.minimize());
    if ($('win-max')) $('win-max').addEventListener('click', () => appWindow.toggleMaximize());
    $('win-close').addEventListener('click', () => appWindow.close());

    // Tabs
    $('tab-clips').addEventListener('click', () => switchTab('clips'));
    $('tab-settings').addEventListener('click', () => switchTab('settings'));

    // Clips gallery toolbar & modals
    $('btn-clips-select').addEventListener('click', () => {
      selectMode = true;
      updateCardSelectionUI();
    });
    $('btn-clips-cancel-select').addEventListener('click', clearClipSelection);
    $('btn-clips-select-all').addEventListener('click', () => {
      if (clips.length === 0) return;
      if (selectedClipPaths.size === clips.length) {
        selectedClipPaths.clear();
      } else {
        clips.forEach((c) => selectedClipPaths.add(c.path));
      }
      updateCardSelectionUI();
    });
    $('btn-clips-delete-bulk').addEventListener('click', () => {
      const list = clips.filter((c) => selectedClipPaths.has(c.path)).map((c) => c.path);
      openDeleteModal(list);
    });
    $('btn-clips-open-folder').addEventListener('click', () => {
      invoke('open_folder', {});
    });
    $('btn-clips-empty-folder').addEventListener('click', () => {
      invoke('open_folder', {});
    });
    $('clips-grid').addEventListener('click', (e) => {
      if (e.target === $('clips-grid') && !selectMode) {
        clearClipSelection();
      }
    });

    $('btn-rename-cancel').addEventListener('click', closeRenameModal);
    $('btn-rename-x').addEventListener('click', closeRenameModal);
    $('btn-rename-save').addEventListener('click', submitRenameModal);
    $('inp-rename-clip').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        submitRenameModal();
      }
    });

    $('btn-delete-cancel').addEventListener('click', closeDeleteModal);
    $('btn-delete-x').addEventListener('click', closeDeleteModal);
    $('btn-delete-confirm').addEventListener('click', confirmDeleteModal);

    window.addEventListener('click', () => hideContextMenu());
    window.addEventListener('blur', () => hideContextMenu());
    const contentScrollEl = document.querySelector('.content-scroll');
    if (contentScrollEl) contentScrollEl.addEventListener('scroll', hideContextMenu);

    // Hotkey capture
    const startHotkeyCapture = async () => {
      if (capturingHotkey) return;
      capturingHotkey = true;
      $('hotkey-note').classList.add('is-hidden');
      syncWindowHeight(true);
      renderHotkey();
      await invoke('begin_hotkey_capture');
    };
    $('btn-hotkey').addEventListener('click', startHotkeyCapture);
    $('btn-change-hotkey').addEventListener('click', startHotkeyCapture);

    window.addEventListener('keydown', (e) => {
      if (capturingHotkey) {
        e.preventDefault();
        return;
      }
      const renameOpen = !$('modal-rename-clip').classList.contains('is-hidden');
      const deleteOpen = !$('modal-delete-clips').classList.contains('is-hidden');
      const quitOpen = !$('modal-quit').classList.contains('is-hidden');
      const menuOpen = !$('clip-context-menu').classList.contains('is-hidden');

      if (e.key === 'Escape') {
        if (menuOpen) {
          hideContextMenu();
        } else if (renameOpen) {
          closeRenameModal();
        } else if (deleteOpen) {
          closeDeleteModal();
        } else if (quitOpen) {
          $('modal-quit').classList.add('is-hidden');
        } else if (activeTab === 'clips' && (selectMode || selectedClipPaths.size > 0)) {
          clearClipSelection();
        }
        return;
      }

      if (renameOpen || deleteOpen || quitOpen) return;
      const tag = (document.activeElement && document.activeElement.tagName) || '';
      if (tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA') return;

      if (activeTab === 'clips') {
        if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'a') {
          e.preventDefault();
          if (clips.length > 0) {
            selectMode = true;
            clips.forEach((c) => selectedClipPaths.add(c.path));
            updateCardSelectionUI();
          }
        } else if (e.key === 'Delete' && selectedClipPaths.size > 0) {
          e.preventDefault();
          const list = clips.filter((c) => selectedClipPaths.has(c.path)).map((c) => c.path);
          openDeleteModal(list);
        } else if (e.key === 'F2' && selectedClipPaths.size === 1) {
          e.preventDefault();
          const targetPath = Array.from(selectedClipPaths)[0];
          const clip = clips.find((c) => c.path === targetPath);
          if (clip) openRenameModal(clip);
        }
      }
    });

    // Sliders
    bindRange('slider-seconds', () => cfg.seconds, (v) => { cfg.seconds = v; }, (v) => {
      $('val-seconds').textContent = fmtSecs(v);
      renderEstimatedSize();
    });
    bindRange('slider-bitrate', () => cfg.bitrate_mbps, (v) => { cfg.bitrate_mbps = v; }, (v) => {
      $('val-bitrate').textContent = `${v} Mbps`;
      renderEstimatedSize();
    });
    bindRange('slider-game-vol', () => cfg.game_volume, (v) => { cfg.game_volume = v; }, (v) => {
      $('val-game-vol').textContent = `${v} %`;
    });
    bindRange('slider-desktop-vol', () => cfg.desktop_volume, (v) => { cfg.desktop_volume = v; }, (v) => {
      $('val-desktop-vol').textContent = `${v} %`;
    });
    bindRange('slider-mic-vol', () => cfg.mic_volume, (v) => { cfg.mic_volume = v; }, (v) => {
      $('val-mic-vol').textContent = `${v} %`;
    });
    bindRange('slider-sound-vol', () => cfg.sound_volume, (v) => { cfg.sound_volume = v; }, (v) => {
      $('val-sound-vol').textContent = `${v} %`;
    });

    // FPS segmented
    document.querySelectorAll('#seg-fps .seg-btn').forEach((btn) => {
      btn.addEventListener('click', () => {
        if (!cfg) return;
        const fps = Number(btn.dataset.fps);
        if (cfg.fps !== fps) {
          cfg.fps = fps;
          renderConfig();
          commitConfigNow();
        }
      });
    });

    // Resolution segmented
    document.querySelectorAll('#seg-res .seg-btn').forEach((btn) => {
      btn.addEventListener('click', () => {
        if (!cfg) return;
        const h = Number(btn.dataset.height);
        if ((cfg.max_height || 0) !== h) {
          cfg.max_height = h;
          renderConfig();
          commitConfigNow();
        }
      });
    });

    // Codec segmented
    document.querySelectorAll('#seg-codec .seg-btn').forEach((btn) => {
      btn.addEventListener('click', () => {
        if (!cfg) return;
        const codec = btn.dataset.codec;
        if ((cfg.codec || 'h264') !== codec) {
          cfg.codec = codec;
          renderConfig();
          commitConfigNow();
        }
      });
    });

    // Capture mode segmented
    document.querySelectorAll('#seg-capture .seg-btn').forEach((btn) => {
      btn.addEventListener('click', () => {
        if (!cfg) return;
        const mode = btn.dataset.capture;
        if ((cfg.capture_mode || 'games') !== mode) {
          cfg.capture_mode = mode;
          renderConfig();
          commitConfigNow();
        }
      });
    });

    // Folder picker
    $('btn-change-folder').addEventListener('click', async () => {
      try {
        const updated = await invoke('pick_folder');
        if (updated) {
          const changed = !cfg || cfg.output_dir !== updated.output_dir;
          cfg = updated;
          if (changed) {
            resetClipsView();
            clearClipSelection();
          }
          renderConfig();
          recState = await invoke('recorder_state');
          renderState();
          if (changed && activeTab === 'clips') {
            refreshClips({ silent: true });
          }
        }
      } catch (e) {
        showToast('error', 'Folder error', String(e));
      }
    });

    // Audio toggles
    $('chk-game-audio').addEventListener('change', (e) => {
      if (!cfg) return;
      cfg.game_audio = e.target.checked;
      renderConfig();
      commitConfigNow();
    });
    $('chk-desktop-audio').addEventListener('change', (e) => {
      if (!cfg) return;
      cfg.desktop_audio = e.target.checked;
      renderConfig();
      commitConfigNow();
    });
    $('chk-mic-audio').addEventListener('change', async (e) => {
      if (!cfg) return;
      cfg.mic_audio = e.target.checked;
      if (cfg.mic_audio) {
        mics = await invoke('list_mics');
      }
      renderConfig();
      commitConfigNow();
    });

    // Mic device picker & refresh
    $('sel-mic-device').addEventListener('change', async (e) => {
      if (!cfg) return;
      cfg = await invoke('set_mic_device', { deviceId: e.target.value });
      renderConfig();
    });
    $('btn-refresh-mics').addEventListener('click', async () => {
      mics = await invoke('list_mics');
      renderMicList();
    });

    // Sound & Autostart
    $('btn-test-sound').addEventListener('click', () => {
      invoke('test_sound', { volume: cfg ? Number(cfg.sound_volume ?? 100) : 100 });
    });
    $('chk-sound-on-save').addEventListener('change', (e) => {
      if (!cfg) return;
      cfg.sound_on_save = e.target.checked;
      renderConfig();
      commitConfigNow();
    });
    $('chk-autostart').addEventListener('change', async (e) => {
      try {
        const enabled = await invoke('set_autostart', { on: e.target.checked });
        e.target.checked = Boolean(enabled);
      } catch (_) {
        e.target.checked = Boolean(recState && recState.autostart);
      }
    });

    // Data folder
    $('btn-open-data').addEventListener('click', () => {
      invoke('open_folder', { target: 'data' });
    });

    // Clip sharing settings
    const bindShareField = (id, key) => {
      const el = $(id);
      if (!el) return;
      el.addEventListener('input', () => {
        if (!cfg) return;
        cfg[key] = el.value.trim();
        scheduleConfigCommit();
      });
      el.addEventListener('change', async () => {
        if (!cfg) return;
        cfg[key] = el.value.trim();
        await commitConfigNow();
        renderConfig();
      });
    };
    bindShareField('inp-share-url', 'share_url');

    // The upload key is write-only: send a replacement (or an explicit clear), never read it.
    const commitShareKey = async (value) => {
      const key = String(value || '').trim();
      try {
        cfg = await invoke('set_share_key', { shareKey: key });
        const el = $('inp-share-key');
        if (el) el.value = '';
        renderConfig();
        showToast('info', key ? 'Upload key saved' : 'Upload key cleared', '');
      } catch (e) {
        showToast('error', "Couldn't save upload key", String(e));
      }
    };

    const toggleKeyBtn = $('btn-toggle-share-key');
    const shareKeyInp = $('inp-share-key');
    if (toggleKeyBtn && shareKeyInp) {
      toggleKeyBtn.addEventListener('click', () => {
        const show = shareKeyInp.type === 'password';
        shareKeyInp.type = show ? 'text' : 'password';
        toggleKeyBtn.textContent = show ? 'Hide' : 'Show';
      });
    }
    if (shareKeyInp) {
      // Committing an empty field must never wipe a configured key: only the Clear button does.
      shareKeyInp.addEventListener('change', () => {
        const key = shareKeyInp.value.trim();
        if (key) commitShareKey(key);
      });
      shareKeyInp.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          const key = shareKeyInp.value.trim();
          if (key) commitShareKey(key);
        }
      });
    }
    const clearShareKeyBtn = $('btn-clear-share-key');
    if (clearShareKeyBtn) {
      clearShareKeyBtn.addEventListener('click', () => {
        if (!cfg || !cfg.has_share_key) return;
        commitShareKey('');
      });
    }

    // Status bar controls
    $('chk-status-enabled').addEventListener('change', async () => {
      recState = await invoke('pause_resume');
      renderState();
    });
    $('btn-start-recorder').addEventListener('click', async () => {
      recState = await invoke('start_recorder');
      renderState();
    });

    // Quit confirmation modal
    $('btn-quit').addEventListener('click', () => {
      $('modal-quit').classList.remove('is-hidden');
    });
    $('btn-modal-cancel').addEventListener('click', () => {
      $('modal-quit').classList.add('is-hidden');
    });
    $('btn-modal-x').addEventListener('click', () => {
      $('modal-quit').classList.add('is-hidden');
    });
    $('btn-modal-exit').addEventListener('click', async () => {
      await commitConfigNow();
      await invoke('quit_app');
    });

    // Refresh clip count (and clips gallery when open) on focus
    window.addEventListener('focus', async () => {
      try {
        recState = await invoke('recorder_state');
        renderState();
        if (activeTab === 'clips') {
          refreshClips({ silent: true });
        }
      } catch (_) {}
    });
  }

  async function init() {
    bindEvents();
    appWindow.onResized(syncMaximizedState);
    syncMaximizedState();

    await listen('state-changed', (event) => {
      recState = event.payload;
      renderState();
    });

    await listen('clips-changed', () => {
      if (activeTab === 'clips') {
        refreshClips({ silent: true });
      } else {
        clipsLoadedOnce = false;
      }
    });

    await listen('config-changed', (event) => {
      if (!draggingSlider && !saveTimer) {
        const prevDir = cfg ? cfg.output_dir : null;
        cfg = event.payload;
        renderConfig();
        if (cfg && cfg.output_dir !== prevDir) {
          resetClipsView();
          if (activeTab === 'clips') {
            refreshClips({ silent: true });
          }
        }
      }
    });

    await listen('clip-upload-state', (event) => {
      const p = event.payload || {};
      if (!p.path) return;
      setClipShareState(p.path, {
        uploading: Boolean(p.uploading),
        uploadedUrl: p.uploaded_url ? p.uploaded_url : undefined,
      });
    });

    await listen('recorder-event', (event) => {
      const p = event.payload || {};
      showToast(p.kind || 'info', p.title || 'clip', p.message || '');
    });

    await listen('hotkey-captured', (event) => {
      const p = event.payload || {};
      capturingHotkey = false;
      renderHotkey();
      if (!p.cancelled && p.note) {
        $('hotkey-note').textContent = p.note;
        $('hotkey-note').classList.remove('is-hidden');
        syncWindowHeight(true);
      }
    });

    let clipsError = null;
    try {
      const [loadedCfg, loadedState, loadedMics, loadedClips] = await Promise.all([
        invoke('get_config'),
        invoke('recorder_state'),
        invoke('list_mics'),
        // A failing gallery must not stop the config and recorder state from initializing.
        invoke('list_clips').catch((e) => {
          clipsError = e;
          return null;
        }),
      ]);

      cfg = loadedCfg;
      recState = loadedState;
      mics = loadedMics || [];

      renderConfig();
      renderState();

      // Clips is the start tab, so its gallery is populated before the first height fit.
      clips = Array.isArray(loadedClips) ? loadedClips : [];
      clipsLoadedOnce = true;
      clipsLoadedDir = cfg.output_dir;
      renderClipsGallery();
      if (clipsError) {
        showToast('error', "Couldn't load clips", String(clipsError));
      }

      uiReady = true;
      await syncWindowHeight(false);
      verifyClipsWindowHeight();
    } catch (e) {
      showToast('error', 'Initialization error', String(e));
    }
  }

  document.addEventListener('DOMContentLoaded', init);
})();
