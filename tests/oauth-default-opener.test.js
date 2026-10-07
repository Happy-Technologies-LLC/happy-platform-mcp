import { jest } from '@jest/globals';

const spawn = jest.fn(() => ({ unref: () => {} }));
jest.unstable_mockModule('node:child_process', () => ({ spawn }));
const { performAuthorizationCodeFlow } = await import('../src/oauth-authorization-code.js');

const platform = Object.getOwnPropertyDescriptor(process, 'platform');
const setPlatform = (value) => Object.defineProperty(process, 'platform', { value });

async function launch(authorizeUrl) {
  const err = jest.spyOn(console, 'error').mockImplementation(() => {});
  try {
    await expect(performAuthorizationCodeFlow({
      authorizeUrl, tokenUrl: 'https://idp.example/oauth_token.do', clientId: 'cli', scope: 'useraccount', timeoutMs: 50
    })).rejects.toMatchObject({ code: 'OAUTH_CALLBACK_TIMEOUT' });
  } finally {
    err.mockRestore();
  }
}

describe('default browser opener', () => {
  beforeEach(() => spawn.mockClear());
  afterEach(() => Object.defineProperty(process, 'platform', platform));

  test('on Windows hands the whole URL to the shell URL handler as one argv entry, without a shell', async () => {
    setPlatform('win32');
    await launch('https://dev.service-now.com/oauth_auth.do');
    expect(spawn).toHaveBeenCalledTimes(1);
    const [command, args, options] = spawn.mock.calls[0];
    expect(command).toBe('rundll32.exe');
    expect(args).toHaveLength(2);
    expect(args[0]).toBe('url.dll,FileProtocolHandler');
    const url = new URL(args[1]);
    for (const name of ['response_type', 'client_id', 'redirect_uri', 'code_challenge', 'code_challenge_method', 'state', 'scope']) {
      expect(url.searchParams.get(name)).toBeTruthy();
    }
    expect(options).toMatchObject({ shell: false });
  });

  test.each([['darwin', 'open'], ['linux', 'xdg-open']])('on %s uses %s without a shell', async (os, opener) => {
    setPlatform(os);
    await launch('https://dev.service-now.com/oauth_auth.do');
    expect(spawn.mock.calls[0][0]).toBe(opener);
    expect(spawn.mock.calls[0][2]).toMatchObject({ shell: false });
  });

  test.each(['file:///C:/Windows/System32/calc.exe', 'javascript:alert(1)//x'])('never launches a non-http(s) URL: %s', async (authorizeUrl) => {
    setPlatform('win32');
    await launch(authorizeUrl).catch(() => {});
    expect(spawn).not.toHaveBeenCalled();
  });
});
