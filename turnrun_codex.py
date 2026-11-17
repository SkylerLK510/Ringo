"""Contained Codex subscription profile for exploratory Session controls.

The controller forwards TLS only to chatgpt.com. A selected access token (no refresh
token, API key or owner profile) is piped to the disposable provider process.
Usage is native cumulative thread usage, differenced once per completed turn.
"""
import ipaddress
import json
from pathlib import Path
import re
import socket
import stat

from turnrun import outside_repo, sha256_file, usage_of
from turnrun_native import NativeAttemptError, NativeBroker, NativeProfileError

MODEL = 'gpt-6-luna'
PROCESS_SOURCES = ('turnrun_codex.py', 'turnrun_codex_bridge.py', 'turnrun_native.py',
                   'turnrun_native_bridge.py', 'turnrun_broker.py', 'turnrun_isolation.py')


def subscription_auth(path):
    """Read an existing ChatGPT login without exposing/forwarding its refresh token."""
    path = Path(path)
    if path.is_symlink():
        raise ValueError('Codex authentication must be an ordinary private file')
    path = outside_repo(path, 'Codex subscription authentication')
    info = path.stat()
    if not stat.S_ISREG(info.st_mode) or info.st_size > 65536 or info.st_mode & 0o077:
        raise ValueError('Codex authentication must be a private ordinary file (mode 0600)')
    value = json.loads(path.read_text())
    if value.get('auth_mode') != 'chatgpt' or value.get('OPENAI_API_KEY'):
        raise ValueError('only native ChatGPT login is permitted; API keys are refused')
    tokens = value.get('tokens', {})
    access, account = tokens.get('access_token'), tokens.get('account_id')
    if (not isinstance(access, str) or not access or len(access) > 32768 or any(c.isspace() for c in access)
            or not isinstance(account, str) or not re.fullmatch(r'[a-zA-Z0-9_-]{1,128}', account)):
        raise ValueError('Codex login is missing a subscription access token or account')
    return {'access_token': access, 'account_id': account}


def provider_socket(authority):
    if authority != 'chatgpt.com:443':
        raise ValueError('only the pinned Codex subscription authority is permitted')
    addresses = socket.getaddrinfo('chatgpt.com', 443, type=socket.SOCK_STREAM)
    if not addresses or any(not ipaddress.ip_address(a[4][0]).is_global for a in addresses):
        raise ValueError('provider DNS returned a non-public destination')
    family, kind, protocol, _, address = addresses[0]
    sock = socket.socket(family, kind, protocol)
    try:
        sock.settimeout(10)
        sock.connect(address)
        sock.settimeout(1)
        return sock
    except BaseException:
        sock.close()
        raise


def session_id(events):
    threads = [e.get('result', {}).get('thread', {}).get('id') for e in events if e.get('id') == 2]
    return threads[0] if len(threads) == 1 else None


def turn_id(events):
    turns = [e.get('result', {}).get('turn', {}).get('id') for e in events if e.get('id') == 3]
    return turns[0] if len(turns) == 1 and isinstance(turns[0], str) and turns[0] else None


def retained_usage(events, previous=None):
    previous = previous or dict(input=0, cache_write=0, cache_read=0, output=0)
    sid = session_id(events)
    tid = turn_id(events)
    reports = [e.get('params', {}).get('tokenUsage', {}).get('total') for e in events
               if e.get('method') == 'thread/tokenUsage/updated' and e.get('params', {}).get('threadId') == sid
               and tid is not None and e.get('params', {}).get('turnId') == tid]
    completions = [e for e in events if e.get('method') == 'turn/completed']
    observed, valid = [], bool(reports) and len(completions) == 1
    if len(completions) == 1:
        valid &= (completions[0].get('params', {}).get('threadId') == sid
                  and tid is not None and completions[0].get('params', {}).get('turn', {}).get('id') == tid)
    for counts in reports:
        if not isinstance(counts, dict):
            valid = False
            continue
        fields = ('inputTokens', 'cachedInputTokens', 'outputTokens')
        if any(type(counts.get(k)) is not int or counts[k] < 0 for k in fields):
            valid = False
            continue
        if counts['cachedInputTokens'] > counts['inputTokens']:
            valid = False
            continue
        writes = counts.get('cacheWriteInputTokens', 0)
        if type(writes) is not int or writes < 0:
            valid = False
            continue
        row = {'input': counts['inputTokens']-counts['cachedInputTokens'],
               'cache_read': counts['cachedInputTokens'], 'cache_write': writes, 'output': counts['outputTokens']}
        if any(row[k] < previous[k] for k in row) or observed and any(row[k] < observed[-1][k] for k in row):
            valid = False
        observed.append(row)
    # A missing or decreasing report never becomes a free completed turn. Preserve
    # per-category lower bounds, retaining the full reservation when incomplete.
    delta = {k: max(0, max((r[k] for r in observed), default=previous[k])-previous[k]) for k in previous}
    return {**delta, 'counted': valid, 'uncounted_calls': 0 if valid else 1}


class CodexBroker(NativeBroker):
    BRIDGE = 'turnrun_codex_bridge.py'
    REPLAY_MODEL = MODEL
    credential = staticmethod(subscription_auth)
    connect_provider = staticmethod(provider_socket)

    def probe(self):
        result = super().probe()
        if result.get('transport_sha256') != sha256_file(Path(__file__).with_name('turnrun_native_bridge.py')):
            raise ValueError('Codex image transport differs from the selected controller source')
        return result

    def validate_model(self, model):
        if model != MODEL:
            raise ValueError('the Codex controls profile pins gpt-6-luna at low reasoning')

    def validate_event(self, event, model):
        if event.get('id') == 2 and 'result' in event:
            result = event['result']
            if (result.get('model') != model or result.get('modelProvider') != 'openai'
                    or result.get('reasoningEffort') != 'low'):
                raise NativeProfileError('Codex selected an unpinned model or provider')
        params = event.get('params', {})
        item = params.get('item', {})
        if item.get('type') in ('commandExecution', 'fileChange', 'webSearch', 'collabAgentToolCall', 'mcpToolCall'):
            raise NativeProfileError('Codex exposed a tool outside the worker profile')

    def save_error(self, events, path, credential):
        # Raw app-server errors can contain tokens and request text. Fixed diagnostics
        # and complete native usage are sufficient; never persist those errors.
        return False

    def _prior(self, events):
        return getattr(self, '_usage_by_session', {}).get(session_id(events))

    def attempt_error(self, reason, events, exit_code=None):
        error = NativeAttemptError(reason, [])
        error.result = {'outcome': 'failed', 'usage': retained_usage(events, self._prior(events)),
                        'diagnostics': {'reason': reason, 'native_exit_code': exit_code}}
        return error

    def parse_result(self, events, code, model):
        sid = session_id(events)
        if not isinstance(sid, str) or not re.fullmatch(r'[a-zA-Z0-9_-]{1,128}', sid):
            raise NativeProfileError('Codex did not report exactly one native thread')
        thread = next(e['result'] for e in events if e.get('id') == 2)
        if (thread.get('model') != model or thread.get('modelProvider') != 'openai'
                or thread.get('reasoningEffort') != 'low'):
            raise NativeProfileError('Codex selected an unpinned model or provider')
        completions = [e for e in events if e.get('method') == 'turn/completed']
        usage = retained_usage(events, self._prior(events))
        failed = code != 0 or len(completions) != 1 or any('error' in e and 'id' in e for e in events)
        failed |= any(e.get('method') == 'error' for e in events)
        if len(completions) == 1:
            p = completions[0].get('params', {})
            failed |= p.get('threadId') != sid or p.get('turn', {}).get('status') != 'completed'
        replies = [e.get('params', {}).get('item', {}).get('text') for e in events
                   if e.get('method') == 'item/completed'
                   and e.get('params', {}).get('threadId') == sid
                   and turn_id(events) is not None and e.get('params', {}).get('turnId') == turn_id(events)
                   and e.get('params', {}).get('item', {}).get('type') == 'agentMessage']
        failed |= not replies or not isinstance(replies[-1], str) or not replies[-1].strip()
        result = {'outcome': 'done' if not failed and usage['counted'] else 'failed',
                  'usage': usage, 'model': model, 'provider_requests': None}
        if result['outcome'] == 'done':
            result['reply'] = replies[-1]
        else:
            result['diagnostics'] = {'reason': 'Codex completion, usage or profile gate failed', 'native_exit_code': code}
        # Failure cleanup can still consult the old baseline without losing observed
        # usage. Update only after teardown has completed, in run/exercise_session.
        return result

    def _remember(self, result, events):
        if usage_of(result)[1] is not None:
            sid = session_id(events)
            old = self._prior(events) or dict(input=0, cache_write=0, cache_read=0, output=0)
            if not hasattr(self, '_usage_by_session'):
                self._usage_by_session = {}
            self._usage_by_session[sid] = {k: old[k]+result['usage'][k] for k in old}

    def run(self, **kwargs):
        events = []
        callback = kwargs.pop('on_event', None)
        def emit(event):
            events.append(event)
            if callback is not None:
                callback(event)
        result = super().run(on_event=emit, **kwargs)
        self._remember(result, events)
        return result

    def replay(self, *, on_event=None, **kwargs):
        events = []
        def emit(event):
            events.append(event)
            if on_event is not None:
                on_event(event)
        result = super().replay(on_event=emit, **kwargs)
        self._remember(result, events)
        return result

    def exercise_session(self, endpoint, request, *, token_cap=100000, seconds=45, on_event=None, error_file=None):
        return self.replay(endpoint=endpoint, prompt=request['prompt'], resume=request.get('resume'),
                           token_cap=token_cap, seconds=seconds, on_event=on_event, error_file=error_file)
