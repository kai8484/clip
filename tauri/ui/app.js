(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const appWindow = window.__TAURI__.window.getCurrentWindow();

  let cfg = null;
  let recState = null;
  let mics = [];
  // Device enumeration is a WASAPI probe: only run it once the mic picker is actually needed.
  let micsLoaded = false;
  let capturingHotkey = false;
  let draggingSlider = false;
  let saveTimer = null;
  let uiReady = false;
  let resizeToken = 0;
  let activeFitHeight = null;
  let heightVerifyToken = 0;
  let wasMaximized = false;
  let setupState = null;
  let installing = false;
  let updateStatus = null;
  let updateBusy = false;

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
  // Share settings the R2 usage was loaded for, so a URL/key change reloads it instead of showing stale numbers.
  let renderedStatsKey = null;
  let r2Stats = null;
  let r2StatsError = null;
  let r2StatsLoading = false;
  let r2TtlSaving = false;
  let r2Purging = false;
  let r2PurgeScope = 'mine';
  let r2InviteCreating = false;
  let r2RevokeTarget = null;
  let r2Revoking = false;
  const R2_FREE_TIER_FALLBACK = 1e10;
  const R2_TTL_CAPTION_OWNER = 'Applies only to your clips in R2: expired clips stop loading right away, and an hourly sweep deletes them.';
  const R2_TTL_CAPTION_FRIEND = "Applies only to your clips in R2 \u2014 does not change the owner's or other friends' settings.";

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
    // The first-run wizard is a fixed overlay: make sure its card is never vertically clipped.
    const wizard = $('modal-install-wizard');
    if (wizard && !wizard.classList.contains('is-hidden')) {
      const card = wizard.querySelector('.modal-card');
      if (card) {
        return Math.max(raw, Math.ceil(card.getBoundingClientRect().height + 64));
      }
    }
    if (activePage.id === 'page-clips') {
      return Math.min(Math.max(raw, 280), 621);
    }
    return raw;
  }

  async function syncWindowHeight(animate) {
    if (!uiReady) return;
    // Until the first list_clips resolves the Clips page is still empty, so fitting it would
    // shrink the window to the empty-state height right before the gallery arrives.
    if (activeTab === 'clips' && !clipsLoadedOnce && !installWizardOpen()) return;
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
    const tabs = ['clips', 'settings', 'cloudflare'];
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
    if (name === 'settings') {
      // Device enumeration is only needed once the mic picker is visible.
      if (cfg?.mic_audio && !micsLoaded) ensureMicsLoaded();
    }
    if (name === 'cloudflare') {
      renderR2();
      // `renderConfig` skips the usage call while this tab is hidden, so catch up here.
      if (isShareConfigured() && renderedStatsKey !== shareConfigKey()) {
        refreshR2Stats({ silent: true });
      }
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

    // Reconcile in place: delete stale cards first, then order the live ones with `insertBefore`
    // only where they are actually out of place. Untouched cards stay mounted, so their
    // `content-visibility: auto` sizes and decoded image layers survive re-renders.
    for (const [path, card] of Array.from(cardMap)) {
      if (!validPaths.has(path)) {
        card.remove();
        cardRefs.delete(card);
        cardMap.delete(path);
        dropPreviewForPath(path);
      }
    }
    clips.forEach((clip, idx) => {
      let card = cardMap.get(clip.path);
      if (!card) {
        card = createClipCard(clip);
        cardMap.set(clip.path, card);
      }
      updateClipCard(card, clip, idx);
      if (grid.children[idx] !== card) {
        grid.insertBefore(card, grid.children[idx] || null);
      }
    });

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

  function shareConfigKey() {
    return `${(cfg?.share_url || '').trim()}|${cfg?.has_share_key ? '1' : '0'}`;
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
    for (const card of cardMap.values()) {
      updateCardShareUI(card, currentClipForCard(card));
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
    if (micsLoaded) renderMicList();

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
    const shareKey = shareConfigKey();
    if (renderedShareConfig !== null && renderedShareConfig !== shareKey) {
      refreshAllCardShareUI();
      if (clipsLoadedOnce) {
        if (activeTab === 'clips') refreshClips({ silent: true });
        else clipsLoadedOnce = false;
      }
    }
    renderedShareConfig = shareKey;

    // R2 admin calls need both halves, so usage is (re)loaded whenever the credentials behind it change.
    const shareReady = isShareConfigured();
    if (!shareReady) {
      r2Stats = null;
      r2StatsError = null;
      renderedStatsKey = null;
    }
    // The Cloudflare card is invisible on every other tab: skip the usage call and the render there,
    // and leave `renderedStatsKey` stale so `switchTab('cloudflare')` loads it when it is shown.
    if (activeTab === 'cloudflare') {
      if (shareReady && renderedStatsKey !== shareKey) {
        refreshR2Stats({ silent: true });
      }
      renderedStatsKey = shareReady ? shareKey : null;
      renderR2();
    }

    renderEstimatedSize();

    // `renderR2()` refits the window by itself when the Cloudflare tab is the visible one.
    if (activeTab !== 'cloudflare') syncWindowHeight(true);
  }

  // ---- Cloudflare R2 tab ---------------------------------------------------------------------

  // Either half of the credentials is enough to have something to disconnect.
  function r2ConnectedAny() {
    return Boolean(cfg && (cfg.share_url?.trim() || cfg.has_share_key));
  }

  // Whole gigabytes (the 10 GB free tier) read cleaner without decimals.
  function fmtStorageSize(bytes) {
    const n = Math.max(0, Number(bytes || 0));
    if (n >= 1e9) {
      const gb = n / 1e9;
      return `${Number.isInteger(gb) ? gb : gb.toFixed(2)} GB`;
    }
    return fmtBytes(n);
  }

  function fmtTtl(seconds) {
    const s = Math.max(0, Number(seconds || 0));
    if (s === 0) return 'Never';
    if (s % 86400 === 0) return `${s / 86400} d`;
    if (s % 3600 === 0) return `${s / 3600} h`;
    return `${Math.max(1, Math.round(s / 60))} min`;
  }

  function plural(n, one, many) {
    return `${fmtCount(n)} ${Number(n) === 1 ? one : many}`;
  }

  // Everything the storage card shows, resolved into one place so the states cannot disagree.
  function r2StorageView() {
    if (!isShareConfigured()) {
      return {
        summary: 'Not connected',
        detail: 'Connect Cloudflare R2 to see storage usage.',
        tone: 'muted',
        pct: 0,
      };
    }
    if (r2StatsError) {
      return { summary: r2StatsError, detail: '', tone: 'error', pct: 0 };
    }
    if (!r2Stats) {
      return {
        summary: 'Loading\u2026',
        detail: 'Reading bucket usage\u2026',
        tone: 'muted',
        pct: 0,
      };
    }
    const free = Number(r2Stats.free_tier_bytes) > 0 ? Number(r2Stats.free_tier_bytes) : R2_FREE_TIER_FALLBACK;
    const used = Math.max(0, Number(r2Stats.used_bytes || 0));
    const bucketUsed = Math.max(0, Number(r2Stats.bucket_used_bytes || 0));

    if (r2Stats.role === 'friend') {
      const quota = Math.max(0, Number(r2Stats.quota_bytes || 0));
      const detail = `${plural(r2Stats.clip_count, 'clip', 'clips')} \u00b7 ${plural(r2Stats.object_count, 'object', 'objects')} (yours)`;
      if (quota > 0) {
        return {
          summary: `${fmtBytes(used)} of ${fmtStorageSize(quota)} limit used \u00b7 ${fmtStorageSize(Math.max(0, quota - used))} left`,
          detail,
          tone: 'live',
          pct: Math.min(100, (used / quota) * 100),
        };
      }
      return {
        summary: `${fmtBytes(used)} used by you \u00b7 ${fmtStorageSize(Math.max(0, free - bucketUsed))} of ${fmtStorageSize(free)} bucket free tier left`,
        detail,
        tone: 'live',
        pct: free > 0 ? Math.min(100, (bucketUsed / free) * 100) : 0,
      };
    }

    const detail = `You: ${plural(r2Stats.clip_count, 'clip', 'clips')} (${fmtBytes(used)}) \u00b7 Bucket total: ${plural(r2Stats.bucket_clip_count, 'clip', 'clips')} (${fmtBytes(bucketUsed)})`;
    return {
      summary: bucketUsed > free
        ? `${fmtBytes(bucketUsed)} used \u00b7 over ${fmtStorageSize(free)} free tier`
        : `${fmtBytes(bucketUsed)} used \u00b7 ${fmtStorageSize(Math.max(0, free - bucketUsed))} of ${fmtStorageSize(free)} free tier left`,
      detail,
      tone: 'live',
      pct: free > 0 ? Math.min(100, (bucketUsed / free) * 100) : 0,
    };
  }

  function r2ConnectionLabel() {
    if (!isShareConfigured()) {
      return r2ConnectedAny() ? 'Not connected \u2014 set both Share URL and Upload key' : 'Not connected';
    }
    if (r2Stats?.role === 'friend') return `Connected as Friend (${r2Stats.label || 'invite'})`;
    if (r2Stats?.role === 'owner') return 'Connected as Owner';
    return 'Connected';
  }

  // Each friend row is built with createElement/textContent: invite labels are user input.
  function friendMeta(friend) {
    const quota = Math.max(0, Number(friend.quota_bytes || 0));
    const used = quota > 0
      ? `${fmtBytes(friend.used_bytes)} of ${fmtStorageSize(quota)}`
      : `${fmtBytes(friend.used_bytes)}`;
    return `${plural(friend.clip_count, 'clip', 'clips')} \u00b7 ${used} \u00b7 Auto-delete ${fmtTtl(friend.ttl_seconds)}`;
  }

  function renderFriends() {
    const list = $('r2-friends-list');
    if (!list) return;
    list.textContent = '';
    const friends = r2Stats?.role === 'owner' && Array.isArray(r2Stats.friends) ? r2Stats.friends : [];
    if (friends.length === 0) {
      const empty = document.createElement('span');
      empty.className = 'r2-friend-empty';
      empty.textContent = 'No friend invites yet.';
      list.appendChild(empty);
      return;
    }
    for (const friend of friends) {
      const row = document.createElement('div');
      row.className = 'r2-friend-row';

      const main = document.createElement('div');
      main.className = 'r2-friend-main';
      const name = document.createElement('span');
      name.className = 'r2-friend-name';
      name.textContent = friend.label || 'Friend';
      name.title = name.textContent;
      const meta = document.createElement('span');
      meta.className = 'r2-friend-meta';
      meta.textContent = friendMeta(friend);
      main.append(name, meta);

      const revoke = document.createElement('button');
      revoke.type = 'button';
      revoke.className = 'btn btn-secondary btn-sm';
      revoke.textContent = 'Revoke';
      revoke.title = `Revoke the invite for ${friend.label || 'this friend'}`;
      revoke.dataset.revokeId = friend.id;
      revoke.disabled = r2Revoking;

      row.append(main, revoke);
      list.appendChild(row);
    }
  }

  function renderR2() {
    const summary = $('r2-storage-summary');
    if (!summary) return;
    const configured = isShareConfigured();
    const isOwner = r2Stats?.role === 'owner';
    const view = r2StorageView();

    summary.textContent = view.summary;
    summary.title = view.summary;
    summary.classList.toggle('is-muted', view.tone === 'muted');
    summary.classList.toggle('is-error', view.tone === 'error');
    $('r2-storage-detail').textContent = view.detail;

    const bar = $('r2-storage-bar');
    bar.style.width = `${view.pct.toFixed(2)}%`;
    bar.classList.toggle('is-high', view.pct >= 90);

    const refreshBtn = $('btn-r2-refresh');
    refreshBtn.disabled = !configured || r2StatsLoading;
    refreshBtn.textContent = r2StatsLoading ? 'Refreshing\u2026' : 'Refresh';

    // A pasted invite is only needed until both halves of a connection are saved.
    $('card-r2-invite-import').classList.toggle('is-hidden', configured);
    $('r2-connection-status').textContent = r2ConnectionLabel();
    $('btn-r2-disconnect').disabled = !r2ConnectedAny();

    // A TTL the Worker returns but no preset covers must not light up a pill.
    const ttl = r2Stats ? Number(r2Stats.ttl_seconds || 0) : null;
    let presetMatched = false;
    document.querySelectorAll('#seg-r2-ttl .seg-btn').forEach((btn) => {
      const isActive = ttl !== null && Number(btn.dataset.ttl) === ttl;
      if (isActive) presetMatched = true;
      btn.classList.toggle('is-active', isActive);
      btn.disabled = !configured || r2TtlSaving || r2StatsLoading;
    });
    const ttlCaption = r2Stats?.role === 'friend' ? R2_TTL_CAPTION_FRIEND : R2_TTL_CAPTION_OWNER;
    $('r2-ttl-hint').textContent = ttl !== null && !presetMatched
      ? `Custom (${fmtTtl(ttl)}) \u00b7 ${ttlCaption}`
      : ttlCaption;

    // Friends only ever delete their own clips; the bucket-wide wipe is Owner-only.
    const mineBtn = $('btn-r2-delete-mine');
    const allBtn = $('btn-r2-delete-all');
    mineBtn.classList.toggle('is-hidden', !configured);
    mineBtn.classList.toggle('btn-danger', isOwner);
    mineBtn.classList.toggle('btn-danger-solid', !isOwner);
    mineBtn.disabled = !configured || r2Purging;
    mineBtn.textContent = r2Purging && r2PurgeScope === 'mine'
      ? 'Deleting\u2026'
      : (isOwner ? 'Delete my clips' : 'Delete my clips in R2');

    allBtn.classList.toggle('is-hidden', !(configured && isOwner));
    allBtn.disabled = !configured || r2Purging;
    allBtn.textContent = r2Purging && r2PurgeScope === 'all'
      ? 'Deleting\u2026'
      : "Delete everybody's clips";

    $('card-r2-invites').classList.toggle('is-hidden', !(configured && isOwner));
    $('btn-r2-create-invite').disabled = !configured || r2InviteCreating;
    $('btn-r2-create-invite').textContent = r2InviteCreating ? 'Creating\u2026' : 'Create & copy invite';
    renderFriends();

    syncWindowHeight(true);
  }

  async function refreshR2Stats({ silent = false } = {}) {
    if (!isShareConfigured()) {
      r2Stats = null;
      r2StatsError = null;
      r2StatsLoading = false;
      renderR2();
      return;
    }
    if (r2StatsLoading) return;
    r2StatsLoading = true;
    r2StatsError = null;
    renderR2();
    try {
      r2Stats = await invoke('get_r2_stats');
      r2StatsError = null;
    } catch (e) {
      r2Stats = null;
      r2StatsError = String(e);
      if (!silent) showToast('error', "Couldn't read R2 storage", String(e));
    } finally {
      r2StatsLoading = false;
      renderR2();
      syncWindowHeight(true);
    }
  }

  async function setR2Ttl(ttl) {
    if (!isShareConfigured() || r2TtlSaving) return;
    if (r2Stats && Number(r2Stats.ttl_seconds || 0) === ttl) return;
    r2TtlSaving = true;
    renderR2();
    try {
      const saved = Number(await invoke('set_r2_ttl', { ttlSeconds: ttl }));
      if (r2Stats) r2Stats.ttl_seconds = saved;
      showToast(
        'info',
        saved === 0 ? 'Auto-delete off' : 'Auto-delete updated',
        saved === 0 ? 'Clips stay in R2 until you delete them.' : `Your clips are deleted from R2 after ${fmtTtl(saved)}.`,
      );
    } catch (e) {
      showToast('error', "Couldn't save auto-delete", String(e));
    } finally {
      r2TtlSaving = false;
      renderR2();
    }
  }

  async function importR2Invite() {
    const inp = $('inp-r2-invite');
    const invite = (inp?.value || '').trim();
    if (!invite) {
      showToast('error', 'Invite code missing', 'Paste the clip-cf: invite code first.');
      return;
    }
    const btn = $('btn-r2-import');
    btn.disabled = true;
    btn.textContent = 'Connecting\u2026';
    try {
      cfg = await invoke('import_cf_invite', { invite });
      if (inp) inp.value = '';
      r2Stats = null;
      r2StatsError = null;
      renderedStatsKey = null;
      renderConfig();
      refreshR2Stats({ silent: true });
      if (clipsLoadedOnce) await refreshClips({ silent: true });
      showToast('info', 'Friend invite connected', 'New clips are uploaded to the shared R2 bucket.');
    } catch (e) {
      showToast('error', "Couldn't use invite code", String(e));
    } finally {
      btn.disabled = false;
      btn.textContent = 'Connect';
    }
  }

  async function createR2Invite() {
    if (r2Stats?.role !== 'owner' || r2InviteCreating) return;
    const labelInp = $('inp-r2-friend-label');
    const label = (labelInp?.value || '').trim();
    if (!label) {
      showToast('error', 'Friend name missing', 'Enter a name for the invite.');
      labelInp?.focus();
      return;
    }
    const quotaBytes = Number($('sel-r2-friend-quota').value || 0);
    r2InviteCreating = true;
    renderR2();
    try {
      const friend = await invoke('create_r2_invite', { label, quotaBytes });
      if (labelInp) labelInp.value = '';
      showToast('info', 'Invite copied', `Send ${friend?.label || label} the invite code \u2014 it's on your clipboard.`);
      await refreshR2Stats({ silent: true });
    } catch (e) {
      showToast('error', "Couldn't create invite", String(e));
    } finally {
      r2InviteCreating = false;
      renderR2();
    }
  }

  function openR2RevokeModal(friend) {
    if (!friend) return;
    r2RevokeTarget = friend;
    $('modal-r2-revoke-title').textContent = `Revoke ${friend.label || 'this friend'}?`;
    $('modal-r2-revoke-subtitle').textContent = friendMeta(friend);
    $('chk-r2-revoke-delete').checked = false;
    $('btn-r2-revoke-confirm').disabled = false;
    $('btn-r2-revoke-confirm').textContent = 'Revoke';
    $('modal-r2-revoke').classList.remove('is-hidden');
    syncWindowHeight(true);
  }

  function closeR2RevokeModal() {
    $('modal-r2-revoke').classList.add('is-hidden');
    r2RevokeTarget = null;
    syncWindowHeight(true);
  }

  async function confirmR2Revoke() {
    if (r2Revoking || !r2RevokeTarget) return;
    const friend = r2RevokeTarget;
    const deleteClips = Boolean($('chk-r2-revoke-delete').checked);
    r2Revoking = true;
    const btn = $('btn-r2-revoke-confirm');
    btn.disabled = true;
    btn.textContent = 'Revoking\u2026';
    try {
      const res = await invoke('revoke_r2_invite', { id: friend.id, deleteClips });
      closeR2RevokeModal();
      const deleted = Number(res?.deleted_clips || 0);
      showToast(
        'info',
        `Revoked ${friend.label || 'the invite'}`,
        deleteClips ? `Deleted ${plural(deleted, 'clip', 'clips')} from R2.` : 'Their clips stay in R2.',
      );
      await refreshR2Stats({ silent: true });
      if (deleteClips && clipsLoadedOnce) await refreshClips({ silent: true });
    } catch (e) {
      closeR2RevokeModal();
      showToast('error', "Couldn't revoke invite", String(e));
    } finally {
      r2Revoking = false;
      btn.disabled = false;
      btn.textContent = 'Revoke';
      renderR2();
    }
  }

  async function openR2PurgeModal(scope) {
    if (!isShareConfigured() || r2Purging) return;
    r2PurgeScope = scope === 'all' ? 'all' : 'mine';
    if (!r2Stats) {
      await refreshR2Stats();
    }
    if (!r2Stats) return;
    const isOwner = r2Stats.role === 'owner';
    const mine = r2PurgeScope === 'mine';
    const clips = mine ? Number(r2Stats.clip_count || 0) : Number(r2Stats.bucket_clip_count || 0);
    const bytes = mine ? Number(r2Stats.used_bytes || 0) : Number(r2Stats.bucket_used_bytes || 0);

    $('modal-r2-purge-title').textContent = mine
      ? 'Delete my clips in R2?'
      : "Delete everybody's clips in R2?";
    $('modal-r2-purge-subtitle').textContent = mine
      ? `You: ${plural(clips, 'clip', 'clips')} \u00b7 ${fmtBytes(bytes)}`
      : `Bucket total: ${plural(clips, 'clip', 'clips')} \u00b7 ${fmtBytes(bytes)}`;
    $('modal-r2-purge-body').innerHTML = mine
      ? `Your clips are deleted from R2 and their cached share links stop working.${
        isOwner ? " Your friends' clips stay untouched." : ''
      }<br /><br />The .mp4 files in your clips folder on this PC are not touched.`
      : 'Every clip in your Cloudflare R2 bucket is deleted \u2014 including clips your friends uploaded \u2014 and the cached share links stop working.<br /><br />The .mp4 files in your clips folder on this PC are not touched.';

    const confirmBtn = $('btn-r2-purge-confirm');
    confirmBtn.disabled = false;
    confirmBtn.textContent = mine ? 'Delete mine' : 'Delete everybody\u2019s';
    $('modal-r2-purge').classList.remove('is-hidden');
    syncWindowHeight(true);
  }

  function closeR2PurgeModal() {
    $('modal-r2-purge').classList.add('is-hidden');
    syncWindowHeight(true);
  }

  async function confirmR2Purge() {
    if (r2Purging || !isShareConfigured()) return;
    const scope = r2PurgeScope;
    r2Purging = true;
    const confirmBtn = $('btn-r2-purge-confirm');
    confirmBtn.disabled = true;
    confirmBtn.textContent = 'Deleting\u2026';
    renderR2();
    try {
      const res = await invoke('delete_all_r2_clips', { scope });
      closeR2PurgeModal();
      const deleted = Number(res?.deleted_objects || 0);
      showToast(
        'info',
        deleted === 1 ? 'Deleted 1 object from R2' : `Deleted ${fmtCount(deleted)} objects from R2`,
        scope === 'all'
          ? 'Cached share links were cleared; local .mp4 files are untouched.'
          : 'Your cached share links were cleared; local .mp4 files are untouched.',
      );
      await refreshR2Stats({ silent: true });
      if (clipsLoadedOnce) await refreshClips({ silent: true });
    } catch (e) {
      closeR2PurgeModal();
      showToast('error', "Couldn't delete R2 clips", String(e));
    } finally {
      r2Purging = false;
      renderR2();
    }
  }

  async function disconnectR2() {
    if (!r2ConnectedAny()) return;
    const btn = $('btn-r2-disconnect');
    btn.disabled = true;
    btn.textContent = 'Disconnecting\u2026';
    try {
      cfg = await invoke('disconnect_r2');
      r2Stats = null;
      r2StatsError = null;
      renderedStatsKey = null;
      renderConfig();
      showToast(
        'info',
        'Disconnected from Cloudflare R2',
        'Clips in R2 and cached share links were left untouched.',
      );
    } catch (e) {
      showToast('error', "Couldn't disconnect", String(e));
    } finally {
      btn.textContent = 'Disconnect';
      renderR2();
    }
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

  // Loads the capture devices on first need and renders the picker; a failure leaves it unloaded so
  // the next tab visit or refresh click retries.
  async function ensureMicsLoaded() {
    if (micsLoaded) return;
    micsLoaded = true;
    try {
      mics = (await invoke('list_mics')) || [];
    } catch (_) {
      micsLoaded = false;
      return;
    }
    renderMicList();
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

  function installWizardOpen() {
    return !$('modal-install-wizard').classList.contains('is-hidden');
  }

  function setInstallError(message) {
    const el = $('install-error');
    el.textContent = message || '';
    el.classList.toggle('is-hidden', !message);
    syncWindowHeight(false);
  }

  function setInstallBusy(busy) {
    installing = busy;
    $('btn-install-confirm').disabled = busy;
    $('btn-install-confirm').textContent = busy ? 'Installing…' : 'Install';
    $('btn-install-browse').disabled = busy;
    $('btn-install-exit').disabled = busy;
    $('inp-install-dir').disabled = busy;
    $('chk-install-shortcut').disabled = busy;
    $('chk-install-autostart').disabled = busy;
  }

  function openInstallWizard(state) {
    setupState = state || null;
    $('inp-install-dir').value = (state && state.default_install_dir) || '';
    $('chk-install-shortcut').checked = true;
    $('chk-install-autostart').checked = true;
    setInstallBusy(false);
    // Nothing is recorded yet, so the background status bar must not claim otherwise.
    $('status-text').textContent = 'Setup required';
    $('status-dot').classList.add('is-muted');
    $('status-toggle-wrap').classList.add('is-hidden');
    $('status-hotkey-hint').textContent = '';
    $('status-meta').textContent = '';
    $('modal-install-wizard').classList.remove('is-hidden');
    setInstallError('');
    // The pill must never sit over the wizard.
    renderUpdateUI();
    requestAnimationFrame(() => {
      syncWindowHeight(false);
      const inp = $('inp-install-dir');
      inp.focus();
      inp.select();
    });
  }

  function closeInstallWizard() {
    $('modal-install-wizard').classList.add('is-hidden');
    renderUpdateUI();
    syncWindowHeight(true);
  }

  async function browseInstallFolder() {
    if (installing) return;
    const current = $('inp-install-dir').value.trim() || (setupState && setupState.default_install_dir) || '';
    try {
      const picked = await invoke('pick_install_folder', { current });
      if (picked) {
        $('inp-install-dir').value = picked;
        setInstallError('');
      }
    } catch (e) {
      setInstallError(String(e));
    }
  }

  async function reloadAfterInstall() {
    try {
      const [loadedCfg, loadedState, loadedClips] = await Promise.all([
        invoke('get_config'),
        invoke('recorder_state'),
        invoke('list_clips').catch((e) => {
          showToast('error', "Couldn't load clips", String(e));
          return null;
        }),
      ]);
      cfg = loadedCfg;
      recState = loadedState;
      renderConfig();
      renderState();
      invoke('get_update_status')
        .then((status) => {
          if (status) updateStatus = status;
          renderUpdateUI();
        })
        .catch(() => {});
      clips = Array.isArray(loadedClips) ? loadedClips : [];
      clipsLoadedOnce = true;
      clipsLoadedDir = cfg ? cfg.output_dir : null;
      renderClipsGallery();
    } catch (e) {
      showToast('error', 'Initialization error', String(e));
    }
    await syncWindowHeight(true);
    verifyClipsWindowHeight();
  }

  async function submitInstall() {
    if (installing) return;
    const installDir = $('inp-install-dir').value.trim();
    if (!installDir) {
      setInstallError('Choose a folder to install clip.');
      return;
    }
    setInstallError('');
    setInstallBusy(true);
    try {
      const res = await invoke('install_app', {
        installDir,
        createShortcut: $('chk-install-shortcut').checked,
        startWithWindows: $('chk-install-autostart').checked,
      });
      if (!res || res.relaunched !== false) {
        // The installed copy is starting and this process exits; keep the wizard in its busy state.
        return;
      }
      $('inp-install-dir').value = res.install_dir || installDir;
      setupState = setupState ? { ...setupState, needs_setup: false } : setupState;
      closeInstallWizard();
      await reloadAfterInstall();
    } catch (e) {
      setInstallBusy(false);
      setInstallError(String(e));
    }
  }

  // ---- Updates ---------------------------------------------------------------------------

  function updatePhase() {
    return (updateStatus && updateStatus.phase) || 'idle';
  }

  function updateStatusText() {
    const s = updateStatus || {};
    switch (s.phase) {
      case 'checking':
        return 'Checking…';
      case 'up_to_date':
        return 'Up to date';
      case 'available':
        return s.latest_version ? `v${s.latest_version} available` : 'Update available';
      case 'downloading':
        return 'Downloading…';
      case 'ready':
        return 'Restarting…';
      case 'error':
        return s.error || "Couldn't check for updates";
      default:
        return 'Not checked yet';
    }
  }

  // Setup must stay untouched: no update chrome while the install wizard owns the window.
  function updatesSuppressed() {
    return installWizardOpen() || Boolean(setupState && setupState.needs_setup);
  }

  function renderUpdateUI() {
    const versionEl = $('txt-update-version');
    const statusEl = $('txt-update-status');
    const btn = $('btn-check-updates');
    const pill = $('btn-titlebar-update');
    if (!versionEl || !statusEl || !btn || !pill) return;

    const phase = updatePhase();
    const s = updateStatus || {};
    const working = phase === 'checking' || phase === 'downloading' || phase === 'ready';
    const canApply = phase === 'available';

    versionEl.textContent = `v${s.current_version || ''}`;
    statusEl.textContent = updateStatusText();
    statusEl.classList.toggle('is-error', phase === 'error');
    statusEl.title = statusEl.textContent;

    if (canApply) {
      btn.textContent = 'Update available';
    } else if (phase === 'checking') {
      btn.textContent = 'Checking…';
    } else if (phase === 'downloading') {
      btn.textContent = 'Downloading…';
    } else if (phase === 'ready') {
      btn.textContent = 'Restarting…';
    } else {
      btn.textContent = 'Check for updates';
    }
    btn.classList.toggle('btn-primary', canApply);
    btn.classList.toggle('btn-secondary', !canApply);
    btn.disabled = working || updateBusy || updatesSuppressed();

    const showPill =
      !updatesSuppressed() && (canApply || phase === 'downloading' || phase === 'ready');
    pill.classList.toggle('is-hidden', !showPill);
    pill.disabled = !canApply;
    if (phase === 'downloading') {
      pill.textContent = 'Downloading…';
    } else if (phase === 'ready') {
      pill.textContent = 'Restarting…';
    } else {
      pill.textContent = 'Update available';
    }
  }

  async function startUpdateDownload() {
    if (updateBusy || updatePhase() !== 'available') return;
    updateBusy = true;
    updateStatus = { ...(updateStatus || {}), phase: 'downloading', error: null };
    renderUpdateUI();
    try {
      updateStatus = await invoke('apply_update');
      renderUpdateUI();
    } catch (e) {
      updateBusy = false;
      updateStatus = { ...(updateStatus || {}), phase: 'error', error: String(e) };
      renderUpdateUI();
      showToast('error', "Couldn't install the update", String(e));
    }
  }

  async function checkForUpdatesNow() {
    if (updateBusy) return;
    const phase = updatePhase();
    if (phase === 'available') {
      startUpdateDownload();
      return;
    }
    if (phase === 'checking' || phase === 'downloading' || phase === 'ready') return;
    updateBusy = true;
    updateStatus = { ...(updateStatus || {}), phase: 'checking', error: null };
    renderUpdateUI();
    try {
      updateStatus = await invoke('check_for_updates');
    } catch (e) {
      updateStatus = { ...(updateStatus || {}), phase: 'error', error: String(e) };
    } finally {
      updateBusy = false;
      renderUpdateUI();
    }
  }

  function bindEvents() {
    // Window controls (titlebar drag handled by data-tauri-drag-region)
    $('win-min').addEventListener('click', () => appWindow.minimize());
    if ($('win-max')) $('win-max').addEventListener('click', () => appWindow.toggleMaximize());
    $('win-close').addEventListener('click', () => appWindow.close());

    // Updates: the titlebar pill downloads and restarts right away, the Settings button either
    // checks or (once a version is known) starts the download.
    $('btn-titlebar-update').addEventListener('click', startUpdateDownload);
    $('btn-check-updates').addEventListener('click', checkForUpdatesNow);

    // Tabs
    $('tab-clips').addEventListener('click', () => switchTab('clips'));
    $('tab-settings').addEventListener('click', () => switchTab('settings'));
    $('tab-cloudflare').addEventListener('click', () => switchTab('cloudflare'));

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
      const purgeOpen = !$('modal-r2-purge').classList.contains('is-hidden');
      const revokeOpen = !$('modal-r2-revoke').classList.contains('is-hidden');
      const installOpen = installWizardOpen();
      const menuOpen = !$('clip-context-menu').classList.contains('is-hidden');

      if (e.key === 'Escape') {
        // The install wizard has no cancel path: installing or Exit are the only ways out.
        if (installOpen) {
          e.preventDefault();
          return;
        }
        if (menuOpen) {
          hideContextMenu();
        } else if (renameOpen) {
          closeRenameModal();
        } else if (deleteOpen) {
          closeDeleteModal();
        } else if (purgeOpen) {
          closeR2PurgeModal();
        } else if (revokeOpen) {
          closeR2RevokeModal();
        } else if (quitOpen) {
          $('modal-quit').classList.add('is-hidden');
        } else if (activeTab === 'clips' && (selectMode || selectedClipPaths.size > 0)) {
          clearClipSelection();
        }
        return;
      }

      if (renameOpen || deleteOpen || quitOpen || purgeOpen || revokeOpen || installOpen) return;
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
      if (cfg.mic_audio && !micsLoaded) {
        mics = (await invoke('list_mics')) || [];
        micsLoaded = true;
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
      mics = (await invoke('list_mics')) || [];
      micsLoaded = true;
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
        // A replaced key may be the one that made the last stats call fail, so retry it right away.
        if (key && isShareConfigured()) {
          r2Stats = null;
          r2StatsError = null;
          refreshR2Stats({ silent: true });
        }
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

    // Cloudflare R2 tab: connection, storage, auto-delete, invites and the R2 wipes
    $('btn-r2-refresh').addEventListener('click', () => refreshR2Stats());
    $('btn-r2-disconnect').addEventListener('click', disconnectR2);
    $('btn-r2-import').addEventListener('click', importR2Invite);
    $('inp-r2-invite').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        importR2Invite();
      }
    });
    document.querySelectorAll('#seg-r2-ttl .seg-btn').forEach((btn) => {
      btn.addEventListener('click', () => setR2Ttl(Number(btn.dataset.ttl)));
    });
    $('btn-r2-delete-mine').addEventListener('click', () => openR2PurgeModal('mine'));
    $('btn-r2-delete-all').addEventListener('click', () => openR2PurgeModal('all'));
    $('btn-r2-purge-cancel').addEventListener('click', closeR2PurgeModal);
    $('btn-r2-purge-x').addEventListener('click', closeR2PurgeModal);
    $('btn-r2-purge-confirm').addEventListener('click', confirmR2Purge);

    $('btn-r2-create-invite').addEventListener('click', createR2Invite);
    $('inp-r2-friend-label').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        createR2Invite();
      }
    });
    // One delegated handler keeps the revoke buttons working as the list re-renders.
    $('r2-friends-list').addEventListener('click', (e) => {
      const btn = e.target.closest('button[data-revoke-id]');
      if (!btn) return;
      const friend = (r2Stats?.friends || []).find((f) => f.id === btn.dataset.revokeId);
      openR2RevokeModal(friend);
    });
    $('btn-r2-revoke-cancel').addEventListener('click', closeR2RevokeModal);
    $('btn-r2-revoke-x').addEventListener('click', closeR2RevokeModal);
    $('btn-r2-revoke-confirm').addEventListener('click', confirmR2Revoke);

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

    // First-run install wizard
    $('btn-install-browse').addEventListener('click', browseInstallFolder);
    $('btn-install-exit').addEventListener('click', () => invoke('quit_app'));
    $('btn-install-confirm').addEventListener('click', submitInstall);
    $('inp-install-dir').addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        submitInstall();
      }
    });

    // Refresh clip count (and clips gallery when open) on focus
    window.addEventListener('focus', async () => {
      // Closing the folder picker returns focus mid-wizard; refreshing there fights the modal.
      if (installWizardOpen()) return;
      if (activeTab === 'clips') {
        // The gallery scan already refreshes the cached count in Rust, so no extra
        // `recorder_state` (whole-folder rescan + registry read) is needed on the clips tab.
        await refreshClips({ silent: true });
        if (recState) {
          recState.clips_count = clips.length;
          renderState();
        }
        return;
      }
      try {
        recState = await invoke('recorder_state');
        renderState();
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
      // A finished upload changes bucket usage; keep the Cloudflare storage numbers honest.
      if (!p.uploading && p.uploaded_url && isShareConfigured()) {
        refreshR2Stats({ silent: true });
      }
    });

    await listen('update-status', (event) => {
      updateStatus = event.payload;
      renderUpdateUI();
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

    // First run (no data\\config.json next to clip.exe): the install wizard takes over the whole
    // window; nothing is loaded, recorded or written until the user installs.
    try {
      const setup = await invoke('get_setup_state');
      if (setup && setup.needs_setup) {
        uiReady = true;
        openInstallWizard(setup);
        return;
      }
    } catch (_) {}

    let clipsError = null;
    try {
      const [loadedCfg, loadedState, loadedClips] = await Promise.all([
        invoke('get_config'),
        invoke('recorder_state'),
        // A failing gallery must not stop the config and recorder state from initializing.
        invoke('list_clips').catch((e) => {
          clipsError = e;
          return null;
        }),
      ]);

      cfg = loadedCfg;
      recState = loadedState;

      renderConfig();
      renderState();
      invoke('get_update_status')
        .then((status) => {
          if (status) updateStatus = status;
          renderUpdateUI();
        })
        .catch(() => {});

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
