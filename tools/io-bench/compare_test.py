"""Protocol checks prevent plausible-looking output from becoming a false comparison."""
import json
from pathlib import Path
import tempfile
import unittest
from compare import parse_result, validate_pair, validate_samples


class ComparisonContract(unittest.TestCase):
    def result(self):
        """Describe a tiny valid run without executing either storage engine."""
        return {"engine": "shep", "layer": "append-file", "backend": "io_uring", "mode": "durable",
                "groups": 1, "iterations": 3, "warmup": 4, "payload_bytes": 4096,
                "chunk_bytes": 65536, "pool_bytes": 30 * 1024 * 1024, "alignment_bytes": 4096,
                "preallocate_bytes": 2 * 1024 * 1024, "verified": True, "fsyncs": 3,
                "wall_ns": 1000, "write_bytes": 12288, "reads": 0,
                "p50_ns": 20, "p99_ns": 20, "p999_ns": 20, "read_p99_ns": 0}

    def test_refuses_a_weaker_durability_boundary(self):
        """One final flush must not be compared with one flush per durable append."""
        result = self.result()
        result["fsyncs"] = 1
        with self.assertRaisesRegex(RuntimeError, "fsyncs"):
            validate_pair(result, "durable", 1, 4096, 3, 4)

    def test_refuses_different_padding_geometry(self):
        """Native 512-byte writes are not the common 4096-byte experiment."""
        result = self.result()
        result["alignment_bytes"] = 512
        with self.assertRaisesRegex(RuntimeError, "alignment_bytes"):
            validate_pair(result, "durable", 1, 4096, 3, 4)

    def test_audits_percentiles_and_sample_counts(self):
        """Independently saved operations must substantiate the printed summary."""
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "latencies.csv"
            path.write_text("operation,latency_ns\nappend,30\nappend,10\nappend,20\n")
            validate_samples(path, self.result())
            result = self.result()
            result["p99_ns"] = 30
            with self.assertRaisesRegex(RuntimeError, "p99_ns"):
                validate_samples(path, result)
            path.write_text("operation,latency_ns\nappend,10\n")
            with self.assertRaisesRegex(RuntimeError, "counts"):
                validate_samples(path, self.result())

    def test_rejects_ambiguous_native_output(self):
        """Native logging is allowed, but multiple result records are ambiguous."""
        record = "SHEP_IO_BENCH " + json.dumps(self.result())
        self.assertEqual(parse_result("startup log\n" + record)["engine"], "shep")
        with self.assertRaisesRegex(RuntimeError, "expected one"):
            parse_result(record + "\n" + record)

if __name__ == "__main__":
    unittest.main()
