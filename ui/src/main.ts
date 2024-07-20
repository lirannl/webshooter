import { checkAuth, checkIdentity, genKeyPair, getCookie } from "./auth";
import "./style.css";

const root = document.createElement("div");
root.style.width = "100vw";
root.style.textAlign = "center";
document.body.appendChild(root);

const keyPair = await genKeyPair();

const authenticated = await checkAuth(keyPair.publicKey);
if (!authenticated) {
    if (await checkIdentity(keyPair.publicKey))
        await getCookie(keyPair);
    else {
        const button = document.createElement("button");
        button.innerText = "Login";
        button.className = "secondary"
        button.id = "loginButton";
        root.appendChild(button);
        button.addEventListener("click", async ev => {
            ev.preventDefault();
            try {
                await getCookie(keyPair);
                button.remove();
            }
            catch (err) {
                console.log(err);
            }
        })
    }
}
