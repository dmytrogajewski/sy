#!/usr/bin/env python3
"""Backport SGLang PR #36418 request-cancellation lifecycle semantics.

Pinned commit d91c3682 drops tokenizer state when a streamed client disconnects,
then suppresses the delayed scheduler abort because that state is absent. The
orphan continues decoding until its output limit. This source transformer keeps
only scheduler-owned identities during cancellation and force-delivers their
delayed abort, while leaving normally completed streams unchanged.
"""

import sys


def replace_once(source: str, old: str, new: str, label: str) -> str:
    count = source.count(old)
    if count != 1:
        raise ValueError(f"expected one {label} anchor, found {count}")
    return source.replace(old, new, 1)


def main(path: str) -> int:
    with open(path, encoding="utf-8") as handle:
        source = handle.read()
    if "obj._dispatched_rids = dispatched_rids.copy()" in source:
        print("ALREADY PATCHED:", path)
        return 0

    source = replace_once(
        source,
        """        self._init_req_state(obj, request)
        try:
            if self.server_args.language_only:""",
        """        self._init_req_state(obj, request)
        try:
            dispatched_rids = set()
            if self.server_args.language_only:""",
        "generate request initialization",
    )
    source = replace_once(
        source,
        """                    self._send_one_request(tokenized_obj)
                    async for response in self._wait_one_response(obj, request):""",
        """                    self._send_one_request(tokenized_obj)
                    dispatched_rids.add(obj.rid)
                    async for response in self._wait_one_response(obj, request):""",
        "single request dispatch",
    )
    source = replace_once(
        source,
        """                    async for response in self._handle_batch_request(obj, request):
                        yield response
        except BaseException:""",
        """                    async for response in self._handle_batch_request(
                        obj, request, dispatched_rids
                    ):
                        yield response
        except (asyncio.CancelledError, GeneratorExit):
            # Record scheduler identities only for a cancelled stream. The
            # response background task also runs after normal completion.
            obj._dispatched_rids = dispatched_rids.copy()
            # Retain dispatched state until the scheduler abort echo removes it.
            self._discard_pending_req_states(obj, dispatched_rids)
            raise
        except BaseException:""",
        "cancellation cleanup",
    )
    source = replace_once(
        source,
        """    async def _handle_batch_request(
        self,
        obj: Union[GenerateReqInput, EmbeddingReqInput],
        request: Optional[fastapi.Request] = None,
    ):""",
        """    async def _handle_batch_request(
        self,
        obj: Union[GenerateReqInput, EmbeddingReqInput],
        request: Optional[fastapi.Request] = None,
        dispatched_rids: Optional[set[str]] = None,
    ):""",
        "batch request signature",
    )
    source = replace_once(
        source,
        """                self._send_batch_request(tokenized_objs)

                # Set up generators for each request in the batch""",
        """                self._send_batch_request(tokenized_objs)
                if dispatched_rids is not None:
                    dispatched_rids.update(item.rid for item in tokenized_objs)

                # Set up generators for each request in the batch""",
        "batch dispatch",
    )
    source = replace_once(
        source,
        """                        self._send_one_request(tokenized_obj)
                        generators.append(self._wait_one_response(tmp_obj, request))""",
        """                        self._send_one_request(tokenized_obj)
                        if dispatched_rids is not None:
                            dispatched_rids.add(tokenized_obj.rid)
                        generators.append(self._wait_one_response(tmp_obj, request))""",
        "sequential batch dispatch",
    )
    source = replace_once(
        source,
        """                self._send_one_request(tokenized_obj)
                await self._wait_one_response(tmp_obj, request).__anext__()""",
        """                self._send_one_request(tokenized_obj)
                if dispatched_rids is not None:
                    dispatched_rids.add(tokenized_obj.rid)
                await self._wait_one_response(tmp_obj, request).__anext__()""",
        "parallel prefix dispatch",
    )
    source = replace_once(
        source,
        """                    self._send_one_request(tokenized_obj)
                    generators.append(self._wait_one_response(tmp_obj, request))""",
        """                    self._send_one_request(tokenized_obj)
                    if dispatched_rids is not None:
                        dispatched_rids.add(tokenized_obj.rid)
                    generators.append(self._wait_one_response(tmp_obj, request))""",
        "parallel sample dispatch",
    )
    source = replace_once(
        source,
        """    def abort_request(self, rid: str = "", abort_all: bool = False):""",
        """    def abort_request(
        self, rid: str = "", abort_all: bool = False, force: bool = False
    ):""",
        "abort signature",
    )
    source = replace_once(
        source,
        """        if (
            not abort_all
            and self.server_args.tokenizer_worker_num == 1""",
        """        if (
            not abort_all
            and not force
            and self.server_args.tokenizer_worker_num == 1""",
        "abort state guard",
    )
    source = replace_once(
        source,
        """            if obj.is_single:
                self.abort_request(obj.rid)
            else:
                for rid in obj.rid:
                    self.abort_request(rid)""",
        """            dispatched_rids = getattr(obj, "_dispatched_rids", None)
            if dispatched_rids is not None:
                for rid in dispatched_rids:
                    self.abort_request(rid, force=True)
            elif obj.is_single:
                self.abort_request(obj.rid)
            else:
                for rid in obj.rid:
                    self.abort_request(rid)""",
        "delayed disconnect abort",
    )
    source = replace_once(
        source,
        """    def _discard_pending_req_states(self, obj):""",
        """    def _discard_pending_req_states(self, obj, dispatched_rids=None):""",
        "pending-state signature",
    )
    source = replace_once(
        source,
        """        for rid in rids:
            self.rid_to_state.pop(rid, None)

    def _should_dispatch_to_encoder(""",
        """        for rid in rids:
            if dispatched_rids is None or rid not in dispatched_rids:
                self.rid_to_state.pop(rid, None)

    def _should_dispatch_to_encoder(""",
        "pending-state cleanup",
    )

    with open(path, "w", encoding="utf-8") as handle:
        handle.write(source)
    print("PATCHED:", path)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1]))
    except (IndexError, OSError, ValueError) as error:
        print("ERROR:", error)
        sys.exit(1)
