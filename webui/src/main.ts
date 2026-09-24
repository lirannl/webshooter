import { checkCookie, checkIdentity, genKeyPair, getCookie, register } from "./auth";
import "./style.css";

import init, { log, start } from "../wasm/pkg/webshooter_wasm";

const root = document.createElement("div");
root.style.width = "100vw";
root.style.textAlign = "center";
document.body.appendChild(root);

const keyPair = await genKeyPair();

/// Resolve `true` when the session ends up authenticated: a valid cookie, a
/// fresh challenge solved (getCookie), or a brand-new registration. Errors
/// from the network or the server propagate to the caller (rejecting the
/// returned promise was how the original executor behaved).
const authenticated: boolean = await (async () => {
  if (await checkCookie(keyPair.publicKey)) return true;
  if (await checkIdentity(keyPair.publicKey)) {
    await getCookie(keyPair);
    return true;
  }

  const displayNameInput = document.createElement("input");
  displayNameInput.type = "text";
  displayNameInput.placeholder = "Display name";
  displayNameInput.id = "displayNameInput";
  root.appendChild(displayNameInput);

  const button = document.createElement("button");
  button.innerText = "Register";
  button.className = "secondary";
  button.id = "registerButton";
  root.appendChild(button);

  // Registration is the only path whose outcome is decided asynchronously by
  // user interaction, so only it needs a promise; the rest are plain awaits.
  return await new Promise<boolean>((resolve) => {
    button.addEventListener("click", async (ev) => {
      ev.preventDefault();
      const displayName = displayNameInput.value.trim();
      if (!displayName) {
        displayNameInput.focus();
        return;
      }
      button.disabled = true;
      try {
        await register(keyPair, displayName);
        displayNameInput.remove();
        button.remove();
        resolve(true);
      } catch (err) {
        if (err instanceof Error) log(err, "error");
        else console.log(err);
        button.disabled = false;
        resolve(false);
      }
    });
  });
})();

if (authenticated) {
  await init();
  start();
}