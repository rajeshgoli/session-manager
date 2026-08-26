const scenarios = document.querySelectorAll('.scenario');
const watchScreen = document.querySelector('#watch-screen');
const createScreen = document.querySelector('#create-screen');
const successScreen = document.querySelector('#success-screen');
const form = document.querySelector('#agent-form');
const contextLine = document.querySelector('#form-context');
const relationship = document.querySelector('#success-relationship');
const globalMenu = document.querySelector('#global-menu');
const repoButton = document.querySelector('#repo-button');
const repoMenu = document.querySelector('#repo-menu');
const repoLoading = document.querySelector('#repo-loading');
const repoEmpty = document.querySelector('#repo-empty');
const repoName = document.querySelector('#repo-name');
const repoPath = document.querySelector('#repo-path');
const repoInherited = document.querySelector('#repo-inherited');
const providerInherited = document.querySelector('#provider-inherited');
const taskInput = document.querySelector('#task-input');
const nameInput = document.querySelector('#name-input');
const createButton = document.querySelector('#create-agent');
const summaryType = document.querySelector('#summary-type');
const summaryRepo = document.querySelector('#summary-repo');
const formError = document.querySelector('#form-error');
const repoField = document.querySelector('#repo-field');
const providerField = document.querySelector('#provider-field');
const taskField = document.querySelector('#task-field');
const repoError = document.querySelector('#repo-error');
const providerError = document.querySelector('#provider-error');
const taskError = document.querySelector('#task-error');

let mode = 'contextual';

function showScreen(target) {
  watchScreen.hidden = target !== 'watch';
  createScreen.hidden = target !== 'create';
  successScreen.hidden = target !== 'success';
}

function setScenarioActive(name) {
  scenarios.forEach((button) => button.classList.toggle('is-active', button.dataset.scenario === name));
}

function clearErrors() {
  formError.hidden = true;
  [repoField, providerField, taskField].forEach((field) => field.classList.remove('has-error'));
  [repoError, providerError, taskError].forEach((error) => { error.hidden = true; });
}

function setProvider(provider) {
  document.querySelectorAll('.provider-option').forEach((button) => {
    button.classList.toggle('is-selected', button.dataset.provider === provider);
  });
  summaryType.textContent = provider === 'claude' ? 'Claude Code' : provider === 'codex-fork' ? 'Codex' : 'Choose type';
}

function setRepo(name, path) {
  repoName.textContent = name || 'Choose repository';
  repoPath.textContent = path || 'Required';
  summaryRepo.textContent = name || 'Choose repository';
  document.querySelectorAll('.repo-option').forEach((button) => {
    button.classList.toggle('is-selected', button.dataset.path === path);
  });
}

function setMode(nextMode) {
  mode = nextMode;
  clearErrors();
  repoLoading.hidden = true;
  repoEmpty.hidden = true;
  repoButton.hidden = false;
  document.querySelector('.segmented').hidden = false;
  taskInput.value = nextMode === 'contextual'
    ? 'Validate the chart API and frontend together, then report the exact evidence.'
    : '';
  nameInput.value = nextMode === 'contextual' ? '981-chart-validation' : '';

  if (nextMode === 'contextual') {
    contextLine.textContent = 'From 981-chart-engineer';
    relationship.textContent = 'Child of 981-chart-engineer';
    repoInherited.hidden = false;
    providerInherited.hidden = false;
    setRepo('981-355-main', '/Users/rajesh/worktrees/981-355-main');
    setProvider('codex-fork');
  } else {
    contextLine.textContent = 'Start a root agent';
    relationship.textContent = 'Root agent';
    repoInherited.hidden = true;
    providerInherited.hidden = true;
    setRepo('', '');
    setProvider('');
  }
  createButton.disabled = false;
  createButton.classList.remove('is-busy');
  createButton.textContent = 'Create agent';
  Array.from(form.elements).forEach((control) => { control.disabled = false; });
  showScreen('create');
}

function applyScenario(name) {
  setScenarioActive(name);
  globalMenu.hidden = true;
  if (name === 'watch') {
    showScreen('watch');
    return;
  }
  if (name === 'success') {
    showScreen('success');
    return;
  }
  setMode(name === 'global' || name === 'empty' ? 'global' : 'contextual');
  if (name === 'loading') {
    repoButton.hidden = true;
    repoLoading.hidden = false;
  } else if (name === 'empty') {
    repoButton.hidden = true;
    repoEmpty.hidden = false;
    createButton.disabled = true;
  } else if (name === 'validation') {
    setRepo('', '');
    setProvider('');
    taskInput.value = '';
    validate();
  } else if (name === 'creating') {
    setBusy();
  }
}

function validate() {
  clearErrors();
  const hasRepo = repoPath.textContent !== 'Required';
  const hasProvider = Boolean(document.querySelector('.provider-option.is-selected'));
  const hasTask = taskInput.value.trim().length > 0;
  if (!hasRepo) { repoField.classList.add('has-error'); repoError.hidden = false; }
  if (!hasProvider) { providerField.classList.add('has-error'); providerError.hidden = false; }
  if (!hasTask) { taskField.classList.add('has-error'); taskError.hidden = false; }
  formError.hidden = hasRepo && hasProvider && hasTask;
  return hasRepo && hasProvider && hasTask;
}

function setBusy() {
  createButton.disabled = true;
  createButton.classList.add('is-busy');
  createButton.textContent = 'Creating agent...';
  Array.from(form.elements).forEach((control) => { control.disabled = true; });
}

scenarios.forEach((button) => button.addEventListener('click', () => applyScenario(button.dataset.scenario)));

document.querySelector('#contextual-new-agent').addEventListener('click', () => { setMode('contextual'); setScenarioActive('contextual'); });
document.querySelector('#global-menu-button').addEventListener('click', () => { globalMenu.hidden = !globalMenu.hidden; });
document.querySelector('#global-new-agent').addEventListener('click', () => { setMode('global'); setScenarioActive('global'); globalMenu.hidden = true; });
document.querySelector('#close-form').addEventListener('click', () => applyScenario('watch'));
document.querySelector('#back-watch').addEventListener('click', () => applyScenario('watch'));
document.querySelector('#open-agent').addEventListener('click', () => applyScenario('watch'));

repoButton.addEventListener('click', () => {
  repoMenu.hidden = !repoMenu.hidden;
  repoButton.setAttribute('aria-expanded', String(!repoMenu.hidden));
});

document.querySelectorAll('.repo-option').forEach((button) => {
  button.addEventListener('click', () => {
    setRepo(button.dataset.name, button.dataset.path);
    repoMenu.hidden = true;
    repoButton.setAttribute('aria-expanded', 'false');
    repoInherited.hidden = true;
    clearErrors();
  });
});

document.querySelectorAll('.provider-option').forEach((button) => {
  button.addEventListener('click', () => {
    setProvider(button.dataset.provider);
    providerInherited.hidden = true;
    clearErrors();
  });
});

form.addEventListener('submit', (event) => {
  event.preventDefault();
  if (!validate()) return;
  setBusy();
  window.setTimeout(() => {
    showScreen('success');
    setScenarioActive('success');
  }, 700);
});

const requestedScenario = new URLSearchParams(window.location.search).get('scenario');
const knownScenario = Array.from(scenarios).some((button) => button.dataset.scenario === requestedScenario);
applyScenario(knownScenario ? requestedScenario : 'watch');
