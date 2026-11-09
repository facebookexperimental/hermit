#!/usr/bin/env python3
"""Unit tests for direct_run.py (S0) and geomean_report.py (S1).

Run: python3 -m unittest benchmarks.test_benchmark_tools -v
(or: python3 benchmarks/test_benchmark_tools.py)
"""

import importlib.util
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load(name):
    spec = importlib.util.spec_from_file_location(name, HERE / f"{name}.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


direct = load("direct_run")
geo = load("geomean_report")


class TestDirectRun(unittest.TestCase):
    def test_fit_slope_recovers_per_call_cost(self):
        xs = [10000.0, 50000.0, 100000.0]
        ys = [1.0 + 40e-6 * x for x in xs]  # 1 s intercept, 40 us/call
        slope, intercept = direct.fit_slope(xs, ys)
        self.assertAlmostEqual(slope * 1e6, 40.0, places=6)
        self.assertAlmostEqual(intercept, 1.0, places=9)

    def test_fit_slope_rejects_single_x(self):
        with self.assertRaises(ValueError):
            direct.fit_slope([5.0, 5.0], [1.0, 1.0])

    def test_median_iqr(self):
        stats = direct.median_iqr([1.0, 2.0, 3.0, 4.0, 5.0])
        self.assertEqual(stats["median"], 3.0)
        self.assertEqual(stats["n"], 5)

    def test_pick_quiet_cpu(self):
        self.assertEqual(direct.pick_quiet_cpu({0: 0.1, 5: 0.9, 9: 0.5}, {0, 5}), 5)
        with self.assertRaises(ValueError):
            direct.pick_quiet_cpu({0: 0.1}, {7})

    def test_bounds_describe_fails_loud(self):
        desc = direct.Bounds(cpu=3, timeout_s=10, memory_mb=1024,
                             output_cap=100).describe()
        self.assertIn("NOT_APPLIED", desc["bound.cgroup"])
        self.assertTrue(desc["bound.wall"].startswith("APPLIED"))


    def test_bounds_for_kvm_gets_cgroup_not_rlimit(self):
        kvm = direct.bounds_for("kvm", cpu=1, timeout_s=10, memory_mb=16384,
                                kvm_memory_max="16G", output_cap=100)
        ptrace = direct.bounds_for("ptrace", cpu=1, timeout_s=10,
                                   memory_mb=16384, kvm_memory_max="16G",
                                   output_cap=100)
        # AS1 (reverie#1225): KVM must see RLIM_INFINITY, memory via cgroup.
        self.assertEqual(kvm.memory_mode, "cgroup")
        self.assertEqual(kvm.cgroup_memory, "16G")
        self.assertIn("APPLIED:cgroup MemoryMax=16G",
                      kvm.describe()["bound.memory"])
        self.assertEqual(ptrace.memory_mode, "rlimit")
        self.assertIn("RLIMIT_AS=16384MiB", ptrace.describe()["bound.memory"])
        cmd = direct.launch_command(["hermit", "run", "--", "/bin/true"], kvm)
        self.assertEqual(cmd[:6], ["systemd-run", "--user", "--scope", "-p",
                                   "MemoryMax=16G", "--"])
        self.assertEqual(cmd[6:], ["hermit", "run", "--", "/bin/true"])
        self.assertEqual(
            direct.launch_command(["hermit", "run"], ptrace), ["hermit", "run"])

    def test_prepare_command(self):
        entry = {"argv": ["/bin/bash", "x.sh", "--run"], "prepare_script": True}
        self.assertEqual(direct.prepare_command(entry),
                         ["/bin/bash", "x.sh", "--prepare"])
        self.assertIsNone(direct.prepare_command({"argv": ["/bin/true"]}))
        self.assertEqual(
            direct.prepare_command({"argv": ["/bin/true"],
                                    "prepare_argv": ["cc", "-o", "x", "x.c"]}),
            ["cc", "-o", "x", "x.c"])

    def test_load_workloads_validation(self):
        spec = {"workloads": [{"name": "a/b", "argv": ["/bin/true"]}]}
        self.assertEqual(direct.load_workloads(spec)[0]["name"], "a/b")
        for bad in ({"workloads": []},
                    {"workloads": [{"name": "x", "argv": []}]},
                    {"workloads": [{"name": "x", "argv": ["/bin/true"]},
                                   {"name": "x", "argv": ["/bin/true"]}]}):
            with self.assertRaises(ValueError):
                direct.load_workloads(bad)


class TestGeomeanReport(unittest.TestCase):
    def test_geomean(self):
        self.assertAlmostEqual(geo.geomean([2.0, 8.0]), 4.0)

    def test_geomean_rejects_nonpositive(self):
        with self.assertRaises(ValueError):
            geo.geomean([1.0, 0.0])

    def test_qualifies_rules(self):
        self.assertTrue(geo.qualifies(10.0, True, 30.0))    # intercept-corrected
        self.assertTrue(geo.qualifies(30.0, False, 30.0))   # long wall
        self.assertFalse(geo.qualifies(29.9, False, 30.0))  # short, uncorrected
        self.assertFalse(geo.qualifies(None, False, 30.0))

    def test_corrected_ratio_subtracts_intercepts(self):
        self.assertAlmostEqual(geo.corrected_ratio(11.0, 1.0, 6.0, 1.0), 2.0)

    def test_build_rows_excludes_startup_and_marks_seed_unmeasured(self):
        results = {"workloads": {
            "startup_true": {"native": {"wall": {"median": 0.001}},
                             "kvm": {"wall": {"median": 0.2}, "status": "ok"}},
            "getpid": {"native": {"slope_us_per_call": 0.07},
                       "kvm": {"slope_us_per_call": 4.2}},
            "tiny": {"native": {"wall": {"median": 0.5}},
                     "kvm": {"wall": {"median": 0.15}}},
        }}
        rows = geo.build_rows(results, 30.0)
        summary = [r for r in rows if r["kind"] == "summary" and r["backend"] == "kvm"]
        self.assertEqual(len(summary), 1)
        self.assertAlmostEqual(summary[0]["geomean"], 60.0)  # 4.2/0.07 only
        self.assertIn("startup_true", summary[0]["excluded"])
        self.assertIn("tiny", summary[0]["excluded"])
        pop = [r for r in rows if r["kind"] == "population"]
        self.assertEqual(len(pop), len(geo.SEED_WORKLOADS))
        self.assertTrue(all(r["status"] == "not_measured" for r in pop))

    def test_intercept_corrected_workload_enters_geomean(self):
        results = {"workloads": {
            "startup_true": {"native": {"wall": {"median": 0.003}},
                             "kvm": {"wall": {"median": 0.115}}},
            "getpid": {"native": {"slope_us_per_call": 0.16},
                       "kvm": {"slope_us_per_call": 6.4}},
            "data-handling/jq-json-transform": {
                "native": {"wall": {"median": 1.003}},
                "kvm": {"wall": {"median": 2.115}}},
        }}
        rows = geo.build_rows(results, 30.0)
        meas = [r for r in rows if r["kind"] == "measurement"
                and r["workload"] == "data-handling/jq-json-transform"]
        self.assertEqual(len(meas), 1)
        self.assertEqual(meas[0]["basis"], "intercept_corrected")
        self.assertAlmostEqual(meas[0]["ratio"], 2.0)  # (2.115-.115)/(1.003-.003)

    def test_seed_population_is_rows_1_to_16_and_20(self):
        self.assertEqual(len(geo.SEED_WORKLOADS), 17)
        self.assertEqual(geo.SEED_WORKLOADS[0], "applications/kvm-python-examples")
        self.assertEqual(geo.SEED_WORKLOADS[-1], "determinism-stress/thread-interleaving")


if __name__ == "__main__":
    unittest.main()
