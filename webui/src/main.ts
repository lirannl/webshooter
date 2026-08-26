import { checkCookie, checkIdentity, genKeyPair, getCookie, register } from "./auth";
import "./style.css";

import init, { log, start } from "../wasm/pkg/webshooter_wasm";

const root = document.createElement("div");
root.style.width = "100vw";
root.style.textAlign = "center";
document.body.appendChild(root);

const keyPair = await genKeyPair();

const authenticated = await new Promise<boolean>(async (resolve, reject) => {
  if (
    await checkCookie(keyPair.publicKey).catch((err) => {
      reject(err);
      return false;
    })
  )
    resolve(true);
  else {
    if (await checkIdentity(keyPair.publicKey)) {
      await getCookie(keyPair).catch(reject);
      resolve(true);
    } else {
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
    }
  }
});

if (authenticated) {
  await init();
  start();
}
