// Settings device certificates (spec 1710 D8b).
import { useState } from 'preact/hooks';
import { html, api, usePoll, clock, config, toast } from './ui.js';

export function DevicesList() {
  const [data, loadError, reload] = usePoll(() => api('/client/devices'), 30000);
  const [asking, setAsking] = useState(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(null);
  const [revoked, setRevoked] = useState(new Set());
  const [signedOut, setSignedOut] = useState(false);
  const identity = data && data.browser_sign_in;
  const key = (device) => JSON.stringify([device.user_id, device.device_key_id]);
  const revoke = async (device) => {
    setBusy(true);
    setError(null);
    try {
      await api(`/client/devices/${encodeURIComponent(device.device_key_id)}?user_id=${encodeURIComponent(device.user_id)}`, { method: 'DELETE' });
      setRevoked((previous) => new Set([...previous, key(device)]));
      setAsking(null);
      if (identity && identity.device_name === device.device_key_id) setSignedOut(true);
      else reload();
      toast(`${device.device_name} revoked`);
    } catch (failure) {
      setError(`Could not complete revocation: ${failure.message}`);
      reload();
    } finally {
      setBusy(false);
    }
  };
  const devices = data ? data.devices.filter((device) => !device.revoked && !revoked.has(key(device))) : [];
  return html`<section class="device-settings" aria-label="Devices">
    <link rel="stylesheet" href=${`/assets/devices.css?v=${config.build_id}`} />
    ${signedOut ? html`<p role="status">This browser's certificate is revoked. <a href="/cdn-cgi/access/logout">Sign in again with email</a>.</p>` : html`
      ${identity ? html`<div class="device-sign-in"><h3>This browser</h3>
        <p>Signed in by ${identity.method === 'certificate'
          ? html`<strong>device certificate · ${identity.device_name}</strong>`
          : html`<strong>email</strong>`}</p></div>` : null}
      <h3>Devices</h3>
      ${error ? html`<p class="device-error" role="alert">${error}</p>` : null}
      ${loadError ? html`<p class="device-error" role="alert">Could not refresh devices: ${loadError.message}
        <button type="button" class="btn sm" onClick=${reload}>Retry</button></p>` : null}
      ${!data && !loadError ? html`<p role="status">Loading devices…</p>` : null}
      ${data && !devices.length ? html`<p class="sub">No enrolled devices.</p>` : null}
      <ul class="device-list">${devices.map((device) => html`<li key=${key(device)}>
        <div class="device-description"><strong>${device.device_name}</strong>
          <span>${device.kind === 'computer' ? 'Computer' : 'Phone'}
            ${identity && identity.device_name === device.device_key_id ? ' · this browser' : ''}
            · ${device.last_seen_at ? `Last used ${clock(device.last_seen_at)}` : 'No recorded use'}</span>
        </div>
        ${asking === key(device) ? html`<div class="device-confirm">
          <span>Revoke ${device.device_name}? It will lose access immediately.</span>
          <div><button type="button" class="btn sm danger" disabled=${busy} onClick=${() => revoke(device)}>${busy ? 'Revoking…' : 'Revoke device'}</button>
            <button type="button" class="btn sm" disabled=${busy} onClick=${() => setAsking(null)}>Cancel</button></div>
        </div>` : html`<button type="button" class="btn sm danger" disabled=${busy} onClick=${() => { setError(null); setAsking(key(device)); }}>Revoke</button>`}
      </li>`)}</ul>
      <div class="device-enroll"><h3>Add a computer</h3>
        <p>On that computer, run <code>sm device enroll &lt;name&gt;</code>, using a unique name such as <code>macbook</code> or <code>studio</code>.
          Open the address printed by the command in Chrome. Email sign-in remains available.</p></div>
    `}
  </section>`;
}
