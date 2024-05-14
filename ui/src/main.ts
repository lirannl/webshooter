import { OidcClient } from "oidc-client-ts";

// const authData: OidcClient | null = await (await fetch("/oidc_data")).json();

// const sessionId = crypto.randomUUID();

// if (!authData) {
//     const response = await fetch("/login", { headers: { "Authorize": `Bearer ${sessionId}` } });
//     if (!response.ok) document.body.innerText = "Unauthorized. Refresh to try again.";
// }

document.body.innerText = "Hello from Webshooter"

export { }