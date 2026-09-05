'use strict';
const controls = document.createElement('section');
document.getElementById('case').prepend(controls);
const confirmed = field('Confirmed value', '', true, controls);
const confirmedCount = counter('Confirmation count', controls);
button('Review saved value', async () => {
  const value = document.querySelector('[aria-label="Saved value"]').value;
  if (await window.accountDialog.confirm(value)) {
    confirmed.value = value;
    confirmedCount();
  }
}, controls);
