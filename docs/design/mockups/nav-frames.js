// Scales the frames of nav-a|b|c.html down to the width of the sheet. The
// layout width of a frame does not change, so the container queries inside
// keep seeing a desktop or a phone. Mockup only, like mock.js.
(function () {
  function fit() {
    document.querySelectorAll('.fit').forEach(box => {
      const frame = box.querySelector('.frame');
      if (!frame) return;
      const k = Math.min(1, box.clientWidth / frame.offsetWidth);
      frame.style.transform = k < 1 ? 'scale(' + k + ')' : '';
      box.style.height = Math.round(frame.offsetHeight * k) + 'px';
    });
  }
  addEventListener('resize', fit);
  addEventListener('load', fit);
  fit();
})();
