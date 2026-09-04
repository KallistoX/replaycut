// Mockup helper: the state switcher and the theme switcher. The only script
// the mockups carry; nothing in here is meant for index.html except the
// three-line matchMedia rule that opens the clip list on wide screens.
(function () {
  const page = document.body;
  const states = (page.dataset.states || '').split(/\s*\|\s*/).filter(Boolean);

  // floating bar, top right
  const bar = document.createElement('div');
  bar.className = 'mockbar';
  bar.innerHTML = '<label>State <select id="mockState"></select></label><label>Theme <select id="mockTheme"><option value="">wardogs</option><option value="../themes/plain.css">plain</option></select></label><a href="../components.html">components</a>';
  document.body.appendChild(bar);
  const stateSel = bar.querySelector('#mockState'), themeSel = bar.querySelector('#mockTheme');
  states.forEach(s => { const [id, label] = s.split(':'); const o = document.createElement('option'); o.value = id; o.textContent = label || id; stateSel.appendChild(o); });

  function applyState(s) {
    page.dataset.state = s;
    document.querySelectorAll('[data-s]').forEach(el => { el.hidden = !el.dataset.s.split(/\s+/).includes(s); });
    document.querySelectorAll('[data-s-class]').forEach(el => {
      // "state:class state2:class2" - adds the class while that state is active
      el.dataset.sClass.split(/\s+/).forEach(pair => { const [st, cls] = pair.split(':'); el.classList.toggle(cls, st === s); });
    });
    try { history.replaceState(null, '', '#' + s); } catch (e) {}
  }
  stateSel.onchange = () => applyState(stateSel.value);
  const fromHash = location.hash.slice(1);
  const initial = states.some(s => s.split(':')[0] === fromHash) ? fromHash : (states[0] || '').split(':')[0];
  stateSel.value = initial; applyState(initial);

  // theme, shared with the component sheet via localStorage
  const link = document.getElementById('theme');
  const applyTheme = v => { link.href = v; link.disabled = !v; themeSel.value = v; try { localStorage.setItem('rc-theme', v ? 'themes/plain.css' : ''); } catch (e) {} };
  themeSel.onchange = () => applyTheme(themeSel.value);
  let saved = ''; try { saved = localStorage.getItem('rc-theme') || ''; } catch (e) {}
  applyTheme(saved ? '../' + saved : '');

  // Clip list: open on wide screens, collapsed below 1000 px (decision 2).
  const list = document.querySelector('details.cliplist');
  if (list) { const mq = matchMedia('(min-width: 1000px)'); const sync = () => { list.open = mq.matches; }; mq.addEventListener('change', sync); sync(); }
})();
