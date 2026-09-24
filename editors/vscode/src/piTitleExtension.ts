// Entry point of dist/pi-title.mjs, the pi extension the pi.dev launcher loads
// with `pi -e` when `omniDevWorktrees.piTabTitle` is `name` (#1899).

import { type PiTitleApi, registerPiTitle } from "./piTitle";

export default function (pi: PiTitleApi): void {
  registerPiTitle(pi, process.env);
}
