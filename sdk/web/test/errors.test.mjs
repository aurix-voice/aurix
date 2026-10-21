import { test } from 'node:test';
import assert from 'node:assert/strict';
import { describeError, isErrorRetryable, isErrorPlayerFacing } from '../dist/index.js';

test('error codes map to advice, retryability and audience', () => {
  assert.match(describeError('TOKEN_EXPIRED'), /mint a new token/);
  assert.match(describeError('CHANNEL_FULL'), /voice is full/);
  assert.match(describeError(undefined), /Voice request failed/);
  assert.equal(isErrorRetryable('CHANNEL_FULL'), true);
  assert.equal(isErrorRetryable('AUTH_DENIED'), false);
  assert.equal(isErrorRetryable(null), false);
  assert.equal(isErrorPlayerFacing('USER_BANNED'), true);
  assert.equal(isErrorPlayerFacing('VALIDATION_ERROR'), false);
});
