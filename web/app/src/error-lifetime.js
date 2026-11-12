// SPDX-License-Identifier: EUPL-1.2
// One current error owner. A retained send completion cannot dismiss a newer
// error, even when its text or conversation is identical.
export class ErrorLifetime {
  current = null;
  replace(conversation = null) {
    this.current = conversation == null ? null : {conversation};
  }
  capture(conversation) {
    return this.current?.conversation === conversation ? this.current : null;
  }
  accepts(lease) {
    return lease != null && lease === this.current;
  }
}
