import { genKeyPair, getCookie } from "./auth";
import "./style.css";

const root = document.createElement("div");
root.style.width = "100vw";
root.style.textAlign = "center";
root.innerText = "Hello from Webshooter";
const clearLocalStorage = document.createElement("button");
clearLocalStorage.innerText = "Clear local storage";
clearLocalStorage.addEventListener("click", ev => {
    ev.preventDefault();
    localStorage.clear()
})

document.body.appendChild(root);
root.appendChild(clearLocalStorage)

const keyPair = await genKeyPair();

const button = document.createElement("button");
button.innerText = "Login";
button.addEventListener("click", async ev => {
    ev.preventDefault();
    try { await getCookie(keyPair); }
    catch (err) {
        console.log(err);
    }
})
root.appendChild(button)