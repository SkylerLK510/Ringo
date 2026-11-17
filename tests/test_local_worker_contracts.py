import json
import unittest

from local_worker.candidate import CandidateError, parse_candidate
from local_worker.config import ConfigError
from local_worker.prompts import (
    TEMPLATE_TOKEN_OVERHEAD,
    AssignmentTooLarge,
    build_prompt,
    validate_prompt_size,
)
from local_worker.schemas import SchemaError, strict_json_loads

from local_worker_support import SLICE_CONTENT, candidate_json, make_assignment, make_config


class AssignmentContractTests(unittest.TestCase):
    def test_digest_is_stable_and_identity_bound(self):
        first = make_assignment()
        second = make_assignment()
        self.assertEqual(first.digest(), second.digest())
        changed = make_assignment(objective="A different objective.")
        self.assertNotEqual(first.digest(), changed.digest())
        self.assertTrue(first.digest().startswith("sha256:"))

    def test_traversal_and_absolute_paths_rejected(self):
        for path in ("../secret.txt", "/etc/passwd", "a/../../b", "a\\b", "a/./b"):
            with self.subTest(path=path):
                with self.assertRaises(SchemaError):
                    make_assignment(allowed_paths=[path])

    def test_unknown_fields_are_rejected(self):
        with self.assertRaises(ConfigError):
            make_config(**{"surprise": "ignored?"})

    def test_strict_json_rejects_duplicate_keys(self):
        with self.assertRaises(SchemaError):
            strict_json_loads('{"a": 1, "a": 2}')

    def test_config_requires_loopback_and_pinned_model(self):
        with self.assertRaises(ConfigError):
            make_config(endpoint="http://example.com:11434")
        with self.assertRaises(ConfigError):
            make_config(model_digest=None)
        self.assertTrue(make_config(model_digest=None, allow_unpinned_model=True).allow_unpinned_model)


class CandidateContractTests(unittest.TestCase):
    def setUp(self):
        self.assignment = make_assignment()
        self.config = make_config()

    def test_malformed_json_is_a_candidate_error(self):
        with self.assertRaises(CandidateError) as ctx:
            parse_candidate("not json", self.assignment, self.config)
        self.assertEqual(ctx.exception.code, "malformed_json")

    def test_stale_preimage_rejected(self):
        payload = json.loads(candidate_json())
        payload["files"][0]["preimage_digest"] = "sha256:" + "0" * 64
        with self.assertRaises(CandidateError) as ctx:
            parse_candidate(json.dumps(payload), self.assignment, self.config)
        self.assertEqual(ctx.exception.code, "stale_preimage")

    def test_unexpected_path_rejected(self):
        payload = json.loads(candidate_json())
        payload["files"][0]["path"] = "../secret.txt"
        with self.assertRaises(CandidateError) as ctx:
            parse_candidate(json.dumps(payload), self.assignment, self.config)
        self.assertEqual(ctx.exception.code, "unsafe_path")

    def test_text_candidate_cannot_carry_files(self):
        payload = json.loads(candidate_json(kind="text"))
        payload["files"] = [{"path": "notes.txt", "content": "x", "preimage_digest": self.assignment.file_slices[0].digest}]
        with self.assertRaises(CandidateError) as ctx:
            parse_candidate(json.dumps(payload), self.assignment, self.config)
        self.assertEqual(ctx.exception.code, "unexpected_files")

    def test_valid_patch_is_accepted(self):
        raw = candidate_json(assignment=self.assignment)
        candidate = parse_candidate(raw, self.assignment, self.config)
        self.assertEqual(candidate.kind, "patch")
        self.assertEqual(candidate.changed_paths(), ("notes.txt",))
        self.assertTrue(candidate.digest().startswith("sha256:"))

    def test_invalid_shapes_are_candidate_errors_not_schema_errors(self):
        # Valid JSON with the wrong shape used to raise SchemaError, which the
        # runner mistakes for bad caller input and never writes a terminal result.
        for raw in ("[]", '{"schema_version":1,"kind":"text"}', '"a string"'):
            with self.subTest(raw=raw):
                with self.assertRaises(CandidateError) as ctx:
                    parse_candidate(raw, self.assignment, self.config)
                self.assertEqual(ctx.exception.code, "invalid_shape")


class PromptSizeTests(unittest.TestCase):
    def test_one_token_per_byte_is_a_true_upper_bound(self):
        # ASCII can be one token per byte for many tokenizers; the old
        # one-token-per-three-bytes estimate let such input pass validation.
        config = make_config(context_tokens=512, max_output_tokens=16)
        prompt = "x" * (512 - 16 - TEMPLATE_TOKEN_OVERHEAD)
        validate_prompt_size(prompt, config)  # exactly at the budget: accepted
        with self.assertRaises(AssignmentTooLarge):
            validate_prompt_size(prompt + "xxxxxxxx", config)

    def test_multibyte_assignment_over_budget_is_rejected(self):
        # The bound counts UTF-8 bytes, so multibyte text cannot sneak through.
        config = make_config(context_tokens=512, max_output_tokens=16)
        assignment = make_assignment(objective="雪" * 300)
        with self.assertRaises(AssignmentTooLarge):
            build_prompt(assignment, config)

    def test_normal_assignment_fits_and_is_not_truncated(self):
        assignment = make_assignment()
        config = make_config()
        prompt = build_prompt(assignment, config)
        self.assertIn(assignment.objective, prompt)
        self.assertIn(SLICE_CONTENT, prompt)
