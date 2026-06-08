import { checkCookie, checkIdentity, genKeyPair, getCookie } from "./auth";
import "./style.css";
import { start } from "./wt";
import "./extensions";

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
      const button = document.createElement("button");
      button.innerText = "Login";
      button.className = "secondary";
      button.id = "loginButton";
      root.appendChild(button);
      button.addEventListener("click", async (ev) => {
        ev.preventDefault();
        try {
          await getCookie(keyPair);
          button.remove();
          resolve(true);
        } catch (err) {
          console.log(err);
          resolve(false);
        }
      });
    }
  }
});

if (authenticated) start();
