/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// VS Code runs this module with --extensionTestsPath (see run.sh). It drives one
// Hermit debug session through VS Code's own debug service, writes what it saw
// to $HERMIT_VSCODE_RESULT, and fails unless the session matched
// $HERMIT_VSCODE_EXPECT: "forward" (a `hermit run --gdbserver` target) or
// "reverse" (a recording served by `hermit replay --dap-listen`).

const vscode = require('vscode');
const fs = require('fs');

const env = process.env;
const BREAK_LINE = 4; // `int y = x * x;` in fixtures/squares.c
const CALL_LINE = 11; // `total += square(i);`
const PRINT_LINE = 13; // `printf(...)`

const messages = [];
const waiters = [];

function onMessage(message) {
  messages.push(message);
  for (const waiter of waiters.slice()) {
    if (waiter.match(message)) {
      waiters.splice(waiters.indexOf(waiter), 1);
      waiter.resolve(message);
    }
  }
}

function next(match, what, ms) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`timed out after ${ms} ms waiting for ${what}`)), ms);
    waiters.push({match, resolve: (message) => { clearTimeout(timer); resolve(message); }});
  });
}

const isStop = (m) => m.type === 'event' && (m.event === 'stopped' || m.event === 'terminated');

async function where(session, threadId) {
  const trace = await session.customRequest('stackTrace', {threadId, levels: 2});
  const frame = trace.stackFrames[0];
  const scopes = await session.customRequest('scopes', {frameId: frame.id});
  const vars = {};
  for (const scope of scopes.scopes.filter((s) => s.name !== 'Registers')) {
    const listed = await session.customRequest('variables', {variablesReference: scope.variablesReference});
    for (const variable of listed.variables) {
      vars[variable.name] = variable.value;
    }
  }
  return {function: frame.name, line: frame.line, vars};
}

function expectStop(result, step, reason, fn, line, name, value) {
  const at = step.at || {};
  if (step.reason !== reason || at.function !== fn || at.line !== line || at.vars[name] !== value) {
    result.failures.push(`${step.action}: expected a ${reason} stop in ${fn} at line ${line} with ` +
      `${name} = ${value}, got ${JSON.stringify(step)}`);
  }
}

exports.run = async function () {
  const result = {expect: env.HERMIT_VSCODE_EXPECT, nonce: env.HERMIT_VSCODE_NONCE, steps: [], failures: []};
  try {
    vscode.debug.registerDebugAdapterTrackerFactory('hermit', {
      createDebugAdapterTracker() {
        return {onDidSendMessage: onMessage};
      },
    });
    vscode.debug.addBreakpoints([new vscode.SourceBreakpoint(new vscode.Location(
      vscode.Uri.file(env.HERMIT_SOURCE), new vscode.Position(BREAK_LINE - 1, 0)))]);
    const config = JSON.parse(env.HERMIT_DEBUG_CONFIG);
    const initialized = next((m) => m.type === 'response' && m.command === 'initialize', 'initialize', 60000);
    const attached = next(isStop, 'the attach stop', 120000);
    if (!await vscode.debug.startDebugging(undefined, config)) {
      throw new Error('vscode.debug.startDebugging returned false');
    }
    result.supportsStepBack = !!(await initialized).body?.supportsStepBack;
    let stop = await attached;
    const session = vscode.debug.activeDebugSession;
    const threadId = stop.body.threadId;
    result.steps.push({action: 'attach', reason: stop.body.reason});

    const resume = async (command, ms) => {
      const stopped = next(isStop, `the stop after ${command}`, ms);
      await session.customRequest(command, {threadId});
      stop = await stopped;
      const step = {action: command, reason: stop.body?.reason};
      if (stop.event === 'stopped') {
        step.at = await where(session, threadId);
      }
      result.steps.push(step);
      return step;
    };
    expectStop(result, await resume('continue', 60000), 'breakpoint', 'square', BREAK_LINE, 'x', '0');
    expectStop(result, await resume('continue', 60000), 'breakpoint', 'square', BREAK_LINE, 'x', '1');
    if (env.HERMIT_VSCODE_EXPECT === 'reverse') {
      if (!result.supportsStepBack) {
        result.failures.push('a recording served by hermit-dap must advertise supportsStepBack');
      } else {
        expectStop(result, await resume('stepBack', 240000), 'step', 'main', CALL_LINE, 'i', '1');
        expectStop(result, await resume('reverseContinue', 240000), 'breakpoint', 'square', BREAK_LINE, 'x', '0');
      }
    } else if (result.supportsStepBack) {
      result.failures.push('a `hermit run --gdbserver` target must not advertise supportsStepBack');
    }

    // VS Code's own breakpoint model must hold only the breakpoint set above.
    result.editorBreakpoints = vscode.debug.breakpoints.map((b) =>
      b.location ? `${b.location.uri.fsPath}:${b.location.range.start.line + 1}` : String(b));
    const expected = `${env.HERMIT_SOURCE}:${BREAK_LINE}`;
    if (JSON.stringify(result.editorBreakpoints) !== JSON.stringify([expected])) {
      result.failures.push(`expected VS Code's breakpoints to be exactly ${expected}, got ` +
        `${result.editorBreakpoints.join(', ') || 'none'}`);
    }
    result.hiddenBreakpointEvents = messages.filter((m) => m.type === 'event' && m.event === 'breakpoint' &&
      !(m.body?.breakpoint?.id > 0)).length;
    if (result.hiddenBreakpointEvents !== 0) {
      result.failures.push(`${result.hiddenBreakpointEvents} breakpoint event(s) named hidden breakpoints`);
    }

    // Positive control for the hidden-breakpoint check: a breakpoint created
    // from the debug console must still reach VS Code as `new`, then `removed`.
    const userEvent = (reason, match) => {
      const wanted = (m) => m.type === 'event' && m.event === 'breakpoint' && m.body?.reason === reason &&
        m.body?.breakpoint?.id > 0 && match(m.body.breakpoint);
      const seen = messages.find(wanted);
      return seen ? Promise.resolve(seen) : next(wanted, `a ${reason} breakpoint event`, 30000);
    };
    await session.customRequest('evaluate', {expression: `break ${env.HERMIT_SOURCE}:${PRINT_LINE}`, context: 'repl'});
    const created = await userEvent('new', (b) => b.line === PRINT_LINE);
    result.editorBreakpointsWithConsoleBreakpoint = vscode.debug.breakpoints.length;
    await session.customRequest('evaluate', {expression: `delete ${created.body.breakpoint.id}`, context: 'repl'});
    await userEvent('removed', (b) => b.id === created.body.breakpoint.id);
    result.consoleBreakpointEvents = 'new and removed';

    const terminated = next((m) => m.type === 'event' && m.event === 'terminated', 'terminated', 30000)
      .catch(() => null);
    await vscode.debug.stopDebugging(session);
    await terminated;
  } catch (error) {
    result.failures.push(String((error && error.stack) || error));
  }
  result.lastMessages = messages.slice(-5).map((m) => JSON.stringify(m).slice(0, 400));
  fs.writeFileSync(env.HERMIT_VSCODE_RESULT, JSON.stringify(result, null, 2));
  if (result.failures.length) {
    throw new Error(result.failures.join('\n'));
  }
};
