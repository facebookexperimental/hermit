/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

const vscode = require('vscode');

function activate(context) {
  context.subscriptions.push(vscode.debug.registerDebugAdapterDescriptorFactory('hermit', {
    createDebugAdapterDescriptor(session) {
      const c = session.configuration;
      if (c.port) {
        return new vscode.DebugAdapterServer(c.port, c.host || '127.0.0.1');
      }
      const args = [];
      if (c.replay) {
        args.push('--replay', c.replay);
        if (c.dataDir) args.push('--data-dir', c.dataDir);
        args.push('--gdbserver-port', String(c.gdbserverPort || 1234));
      }
      return new vscode.DebugAdapterExecutable(c.dapPath || 'hermit-dap', args);
    },
  }));
}

module.exports = { activate, deactivate() {} };
