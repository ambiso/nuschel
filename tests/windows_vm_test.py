#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Host-only HTTP boundary regressions; no Windows, QEMU or Nix required."""
import contextlib
import http.client
import json
import os
from pathlib import Path
import socket
import tempfile
import threading
import unittest
from unittest.mock import patch

import windows_vm as vm


class HarnessTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="windows-http-")
        self.root = Path(self.directory.name)
        self.files = self.root / "files"
        self.files.mkdir()
        (self.files / "fixture.txt").write_bytes(b"private fixture")
        self.installed = self.root / "installed"
        self.jobs = vm.Jobs()
        self.token = "ab" * 32
        self.revision = "cd" * 32
        self.server = vm.HarnessServer(("127.0.0.1", 0), vm.http_handler(
            self.jobs, self.files, self.token, self.installed, self.revision))
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()
        self.directory.cleanup()

    def headers(self):
        return {"Authorization": "Bearer " + self.token, "X-Nuschl-Agent": self.revision}

    def request(self, method, path, body=None, headers=None):
        connection = http.client.HTTPConnection(*self.server.server_address, timeout=3)
        try:
            connection.request(method, path, body=body, headers=headers or {})
            response = connection.getresponse()
            return response.status, response.read()
        finally:
            connection.close()

    def raw(self, data):
        with socket.create_connection(self.server.server_address, timeout=3) as connection:
            connection.sendall(data)
            connection.shutdown(socket.SHUT_WR)
            response = b""
            while chunk := connection.recv(65536):
                response += chunk
            return int(response.split(b" ")[1]), response

    def result(self, value, headers=None):
        return self.request("POST", "/result", json.dumps(value).encode(), {
            **self.headers(), "Content-Type": "application/json", **(headers or {})})

    def test_all_routes_require_credential_before_effects(self):
        job = self.jobs.submit("'private job'")
        for auth in (None, "Bearer " + "00" * 32, "Basic xyz", "Bearer short", "Bearer \u00ff"):
            headers = {"Authorization": auth} if auth is not None else {}
            for method, path in (("GET", "/next"), ("GET", "/files/fixture.txt"), ("POST", "/result")):
                with self.subTest(auth=auth, path=path):
                    status, body = self.request(method, path, b"{}" if method == "POST" else None, headers)
                    self.assertEqual(status, 401)
                    self.assertNotIn(b"private", body)
        self.assertEqual(self.jobs.active[job], "pending")
        self.assertFalse(self.installed.exists())
        self.assertEqual(self.jobs.last_seen, 0)
        self.assertEqual(self.jobs.results, {})

    def test_rotation_and_agent_revision_reject_stale_clients(self):
        status, _ = self.request("GET", "/files/fixture.txt", headers={"Authorization": "Bearer " + "aa" * 32})
        self.assertEqual(status, 401)
        status, _ = self.request("GET", "/next", headers={**self.headers(), "X-Nuschl-Agent": "old"})
        self.assertEqual(status, 409)
        self.assertEqual(self.jobs.last_seen, 0)

    def test_authenticated_roundtrip_and_exactly_once_result(self):
        job = self.jobs.submit("'hello'")
        self.assertEqual(self.result({"id": job, "output": "early"})[0], 409)
        status, body = self.request("GET", "/next", headers=self.headers())
        self.assertEqual(status, 200)
        self.assertEqual(json.loads(body), {"id": job, "script": "'hello'"})
        self.assertTrue(self.installed.exists())
        self.assertEqual(self.result({"id": job, "output": "hello\n"})[0], 200)
        self.assertEqual(self.result({"id": job, "output": "overwrite"})[0], 409)
        self.assertEqual(self.jobs.wait(job, 1), "hello\n")
        self.assertEqual(self.result({"id": job, "output": "late"})[0], 409)

    def test_only_authenticated_flat_regular_files(self):
        status, body = self.request("GET", "/files/fixture.txt", headers=self.headers())
        self.assertEqual((status, body), (200, b"private fixture"))
        outside = self.root / "secret"
        outside.write_bytes(b"outside")
        (self.files / "link.txt").symlink_to(outside)
        (self.files / "directory").mkdir()
        os.mkfifo(self.files / "pipe")
        for name in ("../secret", "%2e%2e%2fsecret", "fixture.txt?token=bad", "fixture.txt#part", "a\\b", "link.txt", "directory", "pipe", ""):
            with self.subTest(name=name):
                self.assertEqual(self.request("GET", "/files/" + name, headers=self.headers())[0], 404)

    def test_reject_framing_before_body_or_effects(self):
        prefix = (f"POST /result HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {self.token}\r\n"
                  f"X-Nuschl-Agent: {self.revision}\r\nContent-Type: application/json\r\n").encode()
        for framing, expected in ((b"", 411), (b"Content-Length: -1\r\n", 400),
                                  (b"Content-Length: abc\r\n", 400),
                                  (b"Content-Length: 0\r\nContent-Length: 0\r\n", 400),
                                  (b"Transfer-Encoding: chunked\r\nContent-Length: 0\r\n", 400),
                                  (f"Content-Length: {vm.MAX_BODY + 1}\r\n".encode(), 413)):
            with self.subTest(framing=framing):
                self.assertEqual(self.raw(prefix + framing + b"\r\n")[0], expected)
        self.assertFalse(self.installed.exists())
        self.assertEqual(self.jobs.last_seen, 0)
        self.assertEqual(self.jobs.results, {})

    def test_duplicate_auth_and_expect_cannot_bypass_validation(self):
        for headers, status in ((f"Authorization: Bearer {self.token}\r\nAuthorization: Bearer {self.token}\r\n", 401),
                                ("Expect: 100-continue\r\nContent-Length: 9999999\r\n", 401),
                                (f"Authorization: Bearer {self.token}\r\nExpect: 100-continue\r\nContent-Length: 9999999\r\n", 413)):
            code, response = self.raw(("POST /result HTTP/1.1\r\nHost: localhost\r\n" + headers + "\r\n").encode())
            self.assertEqual(code, status)
            self.assertNotIn(b"100 Continue", response)
            self.assertNotIn(self.token.encode(), response)

    def test_valid_expect_continue(self):
        job = self.jobs.submit("'hello'")
        self.jobs.next(0)
        body = json.dumps({"id": job, "output": "hello"}).encode()
        head = (f"POST /result HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {self.token}\r\n"
                f"X-Nuschl-Agent: {self.revision}\r\nContent-Type: application/json\r\n"
                f"Content-Length: {len(body)}\r\nExpect: 100-continue\r\n\r\n").encode()
        with socket.create_connection(self.server.server_address, timeout=3) as connection:
            connection.sendall(head)
            interim = connection.recv(1024)
            self.assertIn(b"100 Continue", interim)
            connection.sendall(body)
            connection.shutdown(socket.SHUT_WR)
            self.assertIn(b"200 OK", connection.recv(1024))
        self.assertEqual(self.jobs.wait(job, 1), "hello")

    def test_request_line_and_header_budgets(self):
        self.assertEqual(self.raw(b"GET /" + b"a" * 2050 + b" HTTP/1.1\r\n\r\n")[0], 414)
        self.assertEqual(self.raw(b"GET /next HTTP/1.1\r\nLong: " + b"a" * 8200 + b"\r\n\r\n")[0], 431)
        headers = b"".join(b"Some: " + b"a" * 1000 + b"\r\n" for _ in range(17))
        self.assertEqual(self.raw(b"GET /next HTTP/1.1\r\n" + headers + b"\r\n")[0], 431)
        self.assertEqual(self.jobs.last_seen, 0)

    def test_strict_result_schema_and_body_limit(self):
        job = self.jobs.submit("'hello'")
        self.jobs.next(0)
        invalid = (b"{}", b"[]", b"null", b"{", b"\xff", b'{"id":NaN,"output":"x"}',
                   ('{"id":"%s","id":"%s","output":"x"}' % (job, job)).encode(),
                   json.dumps({"id": job, "output": 1}).encode(),
                   json.dumps({"id": job, "output": "x", "extra": True}).encode(),
                   json.dumps({"id": "../bad", "output": "x"}).encode(),
                   json.dumps({"id": job, "output": "x" * (vm.MAX_OUTPUT + 1)}).encode(),
                   b"[" * 2000 + b"]" * 2000,
                   json.dumps({"id": job, "output": "\ud800"}).encode())
        for body in invalid:
            with self.subTest(body=body[:80]):
                self.assertEqual(self.request("POST", "/result", body, {**self.headers(), "Content-Type": "application/json"})[0], 400)
        self.assertEqual(self.jobs.active[job], "issued")
        self.assertEqual(self.jobs.results, {})
        self.assertEqual(self.request("POST", "/result", b"{}", self.headers())[0], 415)
        self.assertEqual(self.request("POST", "/unknown", b"{}", self.headers())[0], 404)

    def test_cancelled_jobs_and_capacity_are_bounded(self):
        for _ in range(vm.MAX_JOBS * 2):
            job = self.jobs.submit("'timeout'")
            self.assertIsNone(self.jobs.wait(job, 0))
        self.assertEqual(self.jobs.next(0), {})
        jobs = [self.jobs.submit("'wait'") for _ in range(vm.MAX_JOBS)]
        with self.assertRaises(ValueError):
            self.jobs.submit("'overflow'")
        with self.assertRaises(ValueError):
            self.jobs.submit("x" * (vm.MAX_SCRIPT + 1))
        self.assertFalse(self.jobs.complete(jobs[0], "unissued"))
        self.jobs.next(0)
        self.assertIsNone(self.jobs.wait(jobs[0], 0))
        self.assertFalse(self.jobs.complete(jobs[0], "late"))

    def test_existing_unmarked_disk_is_never_reinstalled(self):
        disk = self.root / "disk.qcow2"
        disk.write_bytes(b"irreplaceable image")
        with patch.object(vm, "STATE", self.root), patch.object(vm, "nix_file") as nix:
            with self.assertRaisesRegex(SystemExit, "refusing to reinstall"):
                vm.daemon(object())
            nix.assert_not_called()
        self.assertEqual(disk.read_bytes(), b"irreplaceable image")

    def test_bootstrap_contains_no_credential_and_uses_refresh_only(self):
        command = vm.bootstrap_command()
        self.assertNotIn(self.token, command)
        self.assertIn("NUSCHLVM", command)
        self.assertIn("-RefreshOnly", command)
        self.assertIn(":\\firstlogon.ps1", command)
        for character in command:
            vm.qcodes(character)


if __name__ == "__main__":
    unittest.main()
