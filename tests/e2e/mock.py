"""A scripted OpenAI-compatible /chat/completions server for driving low end to end. Stdlib only.

Usage: python3 mock.py SCRIPT.json PORTFILE REQUESTS.jsonl REPO

SCRIPT.json is a list of replies served in order, one per POST; past its end every reply is {"content": "done."}. Or
it is {"rules": [{"match": text, "reply": {...}}], "after_tool": {...}, "default": {...}}, which answers a tool
result with after_tool and a user message with the first rule whose text it contains. A reply has any of:
  status, headers, body   a non-200 answer (e.g. 429 with a Retry-After header)
  reasoning, content      streamed text, in small deltas
  code                    one run(code) tool call ("calls": [code, ...] for several; "raw_args" for exact arguments)
  finish                  finish_reason (default tool_calls with calls, else stop; "none" sends none)
  no_done, cut_after      omit data: [DONE]; close the connection after this many body bytes
  chunk, crlf, comments   bytes per socket write (default 7, which splits UTF-8 and JSON); \\r\\n line ends;
                          a ': keep-alive' comment after every event
  error_event             a data: {"error": ...} event after the text
  delay                   seconds before answering (and before touch)
  keepalive               [every, seconds]: keep-alive comments every `every` s for `seconds` s before the reply
  touch, after            [path, text] written into REPO before answering (another writer), then `after` s more
  prompt, cached          usage numbers to report instead of the simulated ones

Every request is appended to REQUESTS.jsonl: its index, time, Authorization header, parsed body and raw body. Usage
is simulated with a perfect prefix cache: cached = the longest common prefix of the messages' JSON with any earlier
request's, / 4, floored to 64-token blocks; prompt = the messages' JSON bytes / 4.
"""
import json, os, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

script = json.load(open(sys.argv[1]))
portfile, logfile, repo = sys.argv[2], sys.argv[3], sys.argv[4]
lock = threading.Lock()
state = {'n': 0, 'seen': []}


def lcp(a, b):
    n, i = min(len(a), len(b)), 0
    while i < n and a[i] == b[i]:
        i += 1
    return i


class H(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *a):
        pass

    def do_GET(self):
        body = json.dumps({'data': [{'id': 'mock-model'}]}).encode()
        self.send_response(200)
        self.send_header('content-type', 'application/json')
        self.send_header('content-length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get('content-length') or 0))
        req = json.loads(raw)
        msgs = req.get('messages', [])
        text = ''.join(json.dumps(m, sort_keys=True, ensure_ascii=False) for m in msgs)
        with lock:
            i = state['n']
            state['n'] += 1
            best = max((lcp(text, s) for s in state['seen']), default=0)
            state['seen'].append(text)
            if isinstance(script, dict):
                last = msgs[-1] if msgs else {}
                if last.get('role') == 'tool':
                    reply = script.get('after_tool', {'content': 'Done.'})
                else:
                    said = str(last.get('content', ''))[-4000:]
                    reply = next((r['reply'] for r in script['rules'] if r['match'] in said),
                                 script.get('default', {'content': 'Done.'}))
            else:
                reply = script[i] if i < len(script) else {'content': 'done.'}
            with open(logfile, 'a') as f:
                f.write(json.dumps({'i': i, 'at': time.time(), 'auth': self.headers.get('authorization'), 'body': req,
                                    'raw': raw.decode()}) + '\n')
        time.sleep(reply.get('delay', 0))
        if 'touch' in reply:
            path, body = reply['touch']
            open(os.path.join(repo, path), 'w').write(body)
            time.sleep(reply.get('after', 0))
        if 'status' in reply:
            body = (reply.get('body') or '{"error": {"message": "scripted"}}').encode()
            self.send_response(reply['status'])
            for k, v in (reply.get('headers') or {}).items():
                self.send_header(k, v)
            self.send_header('content-type', 'application/json')
            self.send_header('content-length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        nl = '\r\n' if reply.get('crlf') else '\n'
        events = []

        def ev(delta=None, finish=None, usage=None):
            e = {'id': 'mock', 'object': 'chat.completion.chunk', 'model': 'mock-model',
                 'choices': [{'index': 0, 'delta': delta or {}, 'finish_reason': finish}]}
            if usage is not None:
                e['usage'] = usage
            events.append('data: ' + json.dumps(e, ensure_ascii=False) + nl + nl)
            if reply.get('comments'):
                events.append(': keep-alive' + nl + nl)

        pieces = lambda s, n=5: [s[k:k + n] for k in range(0, len(s), n)]
        for p in pieces(reply.get('reasoning', '')):
            ev({'reasoning_content': p})
        for p in pieces(reply.get('content', '')):
            ev({'content': p})
        calls = [None] if 'raw_args' in reply else reply.get('calls') or ([reply['code']] if 'code' in reply else [])
        for k, code in enumerate(calls):
            args = reply['raw_args'] if code is None else json.dumps({'code': code})
            ev({'tool_calls': [{'index': k, 'id': f'call_{i}_{k}', 'type': 'function',
                                'function': {'name': 'run', 'arguments': ''}}]})
            for p in pieces(args, 11):
                ev({'tool_calls': [{'index': k, 'function': {'arguments': p}}]})
        if reply.get('error_event'):
            events.append('data: ' + json.dumps({'error': reply['error_event']}) + nl + nl)
        prompt = reply.get('prompt', len(text.encode()) // 4)
        cached = reply.get('cached', (best // 4) // 64 * 64)
        usage = {'prompt_tokens': prompt, 'completion_tokens': 10 + len(reply.get('reasoning', '')) // 4,
                 'prompt_tokens_details': {'cached_tokens': cached}, 'prompt_cache_hit_tokens': cached,
                 'prompt_cache_miss_tokens': prompt - cached}
        finish = reply.get('finish') or ('tool_calls' if calls else 'stop')
        if finish != 'none':
            ev({}, finish, usage)
        if not reply.get('no_done'):
            events.append('data: [DONE]' + nl + nl)
        data = ''.join(events).encode()
        if 'cut_after' in reply:
            data = data[:reply['cut_after']]
        self.send_response(200)
        self.send_header('content-type', 'text/event-stream')
        self.send_header('cache-control', 'no-cache')
        self.send_header('connection', 'close')
        self.end_headers()
        step = reply.get('chunk', 7)
        try:
            every, seconds = reply.get('keepalive', (0, 0))
            end = time.time() + seconds
            while time.time() < end:
                self.wfile.write(b': keep-alive\n\n')
                self.wfile.flush()
                time.sleep(every)
            for k in range(0, len(data), step):
                self.wfile.write(data[k:k + step])
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True


srv = ThreadingHTTPServer(('127.0.0.1', 0), H)
open(portfile, 'w').write(str(srv.server_address[1]))
srv.serve_forever()
