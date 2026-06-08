import { send } from "./wt";

import { ClientMessageType } from "./ClientMessage";

declare global {
  interface WebTransport {
    send(message: ClientMessageType[keyof ClientMessageType]): Promise<void>;
    datagramWriter?: WritableStreamDefaultWriter<Uint8Array>;
    datagramReader?: ReadableStreamDefaultReader<Uint8Array>;
  }
}

WebTransport.prototype.send = function (message) {
  return send(this, message);
};
