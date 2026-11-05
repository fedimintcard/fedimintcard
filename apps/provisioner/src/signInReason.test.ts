import {ApiServiceError, ApiUnreachableError} from './api';
import {SIGNIN} from './strings';
import {signInReason} from './ui';

describe('signInReason', () => {
  it('reads a registry 401 as wrong credentials, whatever its wording', () => {
    const e = new ApiServiceError('the operator name or password is not correct', 401);
    expect(SIGNIN.failed(signInReason(e))).toBe(SIGNIN.failed('bad-credentials'));
  });

  it('reads a 429 as rate limited', () => {
    expect(signInReason(new ApiServiceError('slow down', 429))).toBe('login-rate-limited');
  });

  it('keeps unreachable and other refusals apart', () => {
    expect(signInReason(new ApiUnreachableError('x'))).toBe('unreachable');
    expect(signInReason(new ApiServiceError('operator disabled', 403))).toBe('operator disabled');
  });
});
