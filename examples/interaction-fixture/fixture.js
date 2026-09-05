'use strict';
const params = new URLSearchParams(location.search);
const scenario = params.get('case') || 'large-form';
const root = document.getElementById('case');

function button(name, handler, parent = root) {
  const node = document.createElement('button');
  node.type = 'button';
  node.textContent = name;
  node.addEventListener('click', handler);
  parent.append(node);
  return node;
}

function field(name, value = '', readonly = true, parent = root) {
  const label = document.createElement('label');
  label.textContent = name + ' ';
  const node = document.createElement('input');
  node.setAttribute('aria-label', name);
  node.readOnly = readonly;
  node.value = value;
  label.append(node);
  parent.append(label);
  return node;
}

function counter(name, parent = root) {
  const node = field(name, '0', true, parent);
  return () => { node.value = String(Number(node.value) + 1); };
}

function sections(count) {
  for (let i = 0; i < count; i++) {
    const section = document.createElement('section');
    const heading = document.createElement('h2');
    heading.textContent = `Repeated section ${i}`;
    const paragraph = document.createElement('p');
    paragraph.textContent = `Account documentation for section ${i}. ` + 'Reference information. '.repeat(5);
    section.append(heading, paragraph);
    root.append(section);
  }
}

if (scenario === 'large-form' || scenario === 'artifact') {
  const account = field('Account name', '', false);
  const saved = field('Saved value');
  const increment = counter('Submission count');
  button('Save account', () => { saved.value = account.value; increment(); });
  sections(200);
} else if (scenario === 'disabled') {
  const increment = counter('Action count');
  button('Disabled action', increment).disabled = true;
} else if (scenario === 'duplicate' || scenario === 'scoped') {
  for (const name of ['Billing group', 'Shipping group']) {
    const group = document.createElement('section');
    group.setAttribute('role', 'group');
    group.setAttribute('aria-label', name);
    root.append(group);
    button('Duplicate action', counter(name + ' count', group), group);
  }
} else if (scenario === 'delayed') {
  const target = button('Delayed action', counter('Action count'));
  target.hidden = true;
  button('Start delay', () => {
    target.hidden = true;
    setTimeout(() => { target.hidden = false; }, Number(params.get('delay_ms') || 3000));
  });
} else if (scenario === 'moving') {
  const stage = document.createElement('div');
  stage.className = 'stage';
  root.append(stage);
  const target = button('Moving action', counter('Action count'), stage);
  button('Start motion', () => {
    const start = performance.now();
    const duration = Number(params.get('motion_ms') || 3000);
    function move(now) {
      const elapsed = now - start;
      target.style.left = `${Math.min(elapsed / duration, 1) * 350}px`;
      if (elapsed < duration) requestAnimationFrame(move);
    }
    requestAnimationFrame(move);
  });
} else if (scenario === 'occluded' || scenario === 'occluded-distinct') {
  const stage = document.createElement('div');
  stage.className = 'stage';
  root.append(stage);
  button('Covered action', counter('Action count'), stage);
  const cover = button('Cover action', counter('Cover count'), stage);
  cover.style.zIndex = '2';
  if (scenario === 'occluded-distinct') {
    cover.style.left = '55px';
    cover.style.width = '60px';
  }
} else if (scenario === 'mutation') {
  const currentCount = counter('Current count');
  const retiredCount = counter('Retired count');
  const slot = document.createElement('section');
  root.append(slot);
  const original = button('Current action', retiredCount, slot);
  button('Replace target', () => {
    original.textContent = 'Retired action';
    original.style.marginLeft = '280px';
    button('Current action', currentCount, slot);
  });
} else if (scenario === 'iframe' || scenario === 'cross-origin') {
  button('Frame action', counter('Outer count'));
  const frame = document.createElement('iframe');
  frame.title = 'Inner fixture';
  const url = new URL('frame.html', location.href);
  if (scenario === 'cross-origin') url.port = params.get('frame_port');
  frame.src = url.href;
  root.append(frame);
} else {
  throw new Error(`Unknown fixture case: ${scenario}`);
}
function geometry() {
  document.getElementById('geometry').value = `${innerWidth}x${innerHeight}@${devicePixelRatio}`;
}
geometry();
addEventListener('resize', geometry);
document.getElementById('ready').value = 'ready';
