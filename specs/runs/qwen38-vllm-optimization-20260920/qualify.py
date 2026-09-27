"""Bounded, inference-only checks for this signed vLLM rollout."""
import argparse
import concurrent.futures
import hashlib
import importlib.util
import json
import ssl
import time
import urllib.error
import urllib.request
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('mode', choices=['prefix', 'context', 'protocol', 'protocol-sampled', 'edges', 'concurrent', 'sampling'])
parser.add_argument('--base-url', required=True, help='Instance URL ending in /v1')
parser.add_argument('--bearer-file', required=True, type=Path)
parser.add_argument('--ca', required=True, type=Path)
args = parser.parse_args()
spec = importlib.util.spec_from_file_location('benchmark', 'scripts/benchmark-spark-engine.py')
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)
token = args.bearer_file.read_text().strip()
context = ssl.create_default_context(cafile=str(args.ca))
url = args.base_url.rstrip('/') + '/responses'


def generate(extra, stream=True):
    payload = {'model': 'Qwen3.8-Flash-Next-NVFP4',
               'temperature': None if args.mode == 'protocol-sampled' else 0,
               'reasoning': {'effort': 'none'}, 'max_output_tokens': 64,
               'stream': stream, **extra}
    raw = json.dumps(payload).encode()
    request = urllib.request.Request(url, data=raw, headers={
        'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    started = time.monotonic()
    first = None
    text = []
    events = []
    document = None
    try:
        opened = urllib.request.urlopen(request, context=context, timeout=600)
    except urllib.error.HTTPError as error:
        raise RuntimeError(error.read().decode()) from error
    with opened as response:
        if stream:
            for event, data, _ in benchmark.sse_events(response):
                events.append(event)
                if data.get('delta') and first is None:
                    first = time.monotonic()
                if event == 'response.output_text.delta':
                    text.append(data['delta'])
                if event in ('response.completed', 'response.incomplete', 'response.failed'):
                    document = data['response']
        else:
            document = json.load(response)
            text = [part['text'] for item in document.get('output', [])
                    for part in item.get('content', []) if part.get('type') == 'output_text']
    assert document is not None and document.get('status') != 'failed', document
    result = {'request_sha256': hashlib.sha256(raw).hexdigest(),
              'elapsed_ms': round((time.monotonic() - started) * 1000, 3),
              'ttft_ms': None if first is None else round((first - started) * 1000, 3),
              'text': ''.join(text), 'usage': document.get('usage'),
              'status': document.get('status'), 'events': sorted(set(events))}
    return result, document


def emit(kind, result):
    print(json.dumps({'check': kind, **result}, sort_keys=True), flush=True)


def prefix_case(label, repetitions):
    code = 'COPPER-' + label + '-9246'
    prompt = ('Qualification ledger ' + label + '. The access code is ' + code + '.\n'
              + ' xqz' * repetitions
              + '\nReturn only the access code recorded at the start, without formatting.')
    request = {'input': prompt, 'max_output_tokens': 32}
    cold, _ = generate(request)
    emit(label + '-cold', cold)
    assert code in cold['text'], 'cold retrieval failed: ' + label
    warm, _ = generate(request)
    emit(label + '-cached', warm)
    assert warm['text'] == cold['text'], 'cache-hit output differs: ' + label
    assert warm['usage']['input_tokens'] < 262144 - 32
    if label == '262K':
        assert warm['usage']['input_tokens'] >= 260000
    return cold, warm


if args.mode == 'context':
    code = 'COPPER-CONTEXT-9246'
    result, _ = generate({'input': 'The access code is ' + code + '.\n' + ' xqz' * 87000
                          + '\nReturn only the access code recorded at the start.',
                          'max_output_tokens': 32})
    emit('native-context', result)
    assert code in result['text'] and result['usage']['input_tokens'] >= 260000
elif args.mode == 'sampling':
    for concurrency in (2, 4, 8):
        with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
            requests = [{'input': (' xqz' * (7000 if i == 0 else 0))
                         + '\nExplain Linux process scheduling in detail. Topic ' + str(i),
                         'temperature': (None, 0.6, 0)[i % 3],
                         'top_p': (None, 1.0, 0.8)[i % 3],
                         'reasoning': {'effort': 'low' if i % 2 else 'none'},
                         'max_output_tokens': 128} for i in range(concurrency)]
            for result, _ in pool.map(generate, requests):
                emit('sampled-concurrency-' + str(concurrency), result)
                assert result['usage']['output_tokens'] > 0
elif args.mode in ('prefix', 'concurrent'):
    if args.mode == 'prefix':
        for label, repetitions in [('32K', 10600), ('128K', 42600), ('240K', 79800), ('262K', 87000)]:
            prefix_case(label, repetitions)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        futures = [pool.submit(prefix_case, label, 21000) for label in ('CONCURRENT-A', 'CONCURRENT-B')]
        for future in futures:
            future.result()
elif args.mode == 'edges':
    # Run after the prefix suite: aggregate requests exceed cache capacity.
    prefix_case('32K', 10600)
    prefix_case('GROW', 10600)
    prefix_case('GROW', 42600)
    payload = {'model': 'Qwen3.8-Flash-Next-NVFP4', 'input': 'Write a long Rust tutorial.',
               'temperature': 0, 'reasoning': {'effort': 'none'},
               'stream': True, 'max_output_tokens': 1024}
    request = urllib.request.Request(url, data=json.dumps(payload).encode(), headers={
        'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    deltas = 0
    with urllib.request.urlopen(request, context=context, timeout=600) as response:
        for event, data, _ in benchmark.sse_events(response):
            deltas += event == 'response.output_text.delta'
            if deltas == 3:
                break
    assert deltas == 3
    emit('client-disconnect', {'output_deltas_before_close': deltas})
    result, _ = generate({'input': 'Reply with exactly: Spark ready'})
    emit('after-cancellation', result)
    assert result['text'].strip() == 'Spark ready'
else:
    for stream in (False, True):
        result, _ = generate({'input': 'Reply with exactly: Spark ready'}, stream=stream)
        emit('text-stream-' + str(stream), result)
        assert result['text'].strip() == 'Spark ready'
    prompt = 'Call lookup for mem_available_bytes. After the result, reply ADMITTED if it is at least 8589934592; otherwise reply REJECTED.'
    tools = [{'type': 'function', 'name': 'lookup', 'description': 'Read a host metric.',
              'parameters': {'type': 'object', 'properties': {'metric': {'type': 'string'}},
                             'required': ['metric'], 'additionalProperties': False}}]
    result, response = generate({'input': prompt, 'tools': tools, 'tool_choice': 'required', 'max_output_tokens': 128}, stream=False)
    calls = [item for item in response['output'] if item['type'] == 'function_call']
    assert len(calls) == 1 and calls[0]['name'] == 'lookup', response
    call = calls[0]
    assert json.loads(call['arguments'])['metric'] == 'mem_available_bytes'
    emit('tool-call', {**result, 'tool': call})
    history = [{'type': 'message', 'role': 'user', 'content': prompt},
               {key: call[key] for key in ('type', 'call_id', 'name', 'arguments')},
               {'type': 'function_call_output', 'call_id': call['call_id'],
                'output': '{"mem_available_bytes":17179869184}'}]
    result, _ = generate({'input': history, 'tools': tools})
    emit('tool-continuation', result)
    assert result['text'].strip() == 'ADMITTED'
    result, _ = generate({'input': 'What is 17 times 19? Give the result.',
                          'reasoning': {'effort': 'low'}, 'max_output_tokens': 256})
    emit('reasoning', result)
    assert '323' in result['text']
    assert any('reasoning' in event for event in result['events'])
