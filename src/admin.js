// Checks every row box of a list when its header box is checked.
var toggle = document.getElementById('action-toggle');
if (toggle) {
  toggle.addEventListener('change', function (event) {
    document.querySelectorAll('.action-select').forEach(function (box) {
      box.checked = event.target.checked;
    });
  });
}
