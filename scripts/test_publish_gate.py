#!/usr/bin/env python3
"""Decision tests for the release publish gate."""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import publish_gate as gate

ROOT = Path(__file__).resolve().parents[1]

PROD = '''
fn shipped() -> i32 { 1 }

#[cfg(test)]
mod tests {
    fn scratch() { let _ = "helper-body"; }

    #[test]
    fn cfg_test_fn() { let _ = "seam-file-srv"; }
}
'''

PROD_WITH_INNER = '''
fn shipped() -> &'static str {
    let brace = '{';
    let end = "}";
    let _ = brace;
    end
}

#[tokio::test]
async fn only_a_test() { let _ = "seam-file-srv"; }
'''


def _manifest(deps: str, dev: str = "", build: str = "", target_dev: str = "") -> str:
    return f'''
[package]
name = "app"
version = "0.1.0"

[dependencies]
{deps}

[build-dependencies]
{build}

[dev-dependencies]
{dev}

[target.'cfg(windows)'.dev-dependencies]
{target_dev}
'''


def _lock(foo: str = "1.0.0", foo_sum: str = "bbb", bar: str | None = "1.0.0", bar_sum: str = "aaa") -> str:
    packages = f'''
[[package]]
name = "app"
version = "0.1.0"
dependencies = ["foo"{', "bar"' if bar else ""}]

[[package]]
name = "foo"
version = "{foo}"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{foo_sum}"
'''
    if bar:
        packages += f'''
[[package]]
name = "bar"
version = "{bar}"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "{bar_sum}"
'''
    return "version = 3\n" + packages


class PublishGateTests(unittest.TestCase):
    def test_cfg_test_module_edit_does_not_publish(self) -> None:
        before = {"src/lib.rs": PROD}
        after = {"src/lib.rs": PROD.replace("seam-file-srv", "seam-file-srv-edited")}
        self.assertFalse(gate.should_publish(before, after))

    def test_tokio_test_fn_edit_does_not_publish(self) -> None:
        before = {"src/lib.rs": PROD_WITH_INNER}
        after = {"src/lib.rs": PROD_WITH_INNER.replace("seam-file-srv", "changed")}
        self.assertFalse(gate.should_publish(before, after))

    def test_cfg_all_test_is_stripped(self) -> None:
        src = '''
fn shipped() {}
#[cfg(all(test, feature = "extra"))]
fn hidden() { let _ = "only-test"; }
'''
        edited = src.replace("only-test", "changed")
        self.assertFalse(gate.should_publish({"a.rs": src}, {"a.rs": edited}))

    def test_cfg_any_test_is_kept(self) -> None:
        src = '''
#[cfg(any(test, feature = "extra"))]
fn maybe_shipped() { let _ = "maybe"; }
'''
        edited = src.replace("maybe", "changed")
        self.assertTrue(gate.should_publish({"a.rs": src}, {"a.rs": edited}))

    def test_cfg_attr_test_is_kept(self) -> None:
        src = '''
#[cfg_attr(test, allow(dead_code))]
fn shipped() { let _ = "keep"; }
'''
        self.assertTrue(
            gate.should_publish({"a.rs": src}, {"a.rs": src.replace("keep", "changed")})
        )

    def test_removing_cfg_test_publishes(self) -> None:
        after = PROD.replace("#[cfg(test)]\n", "", 1)
        # helper-body was test-only via the cfg(test) module. Dropping the
        # attribute ships it. The #[test] function stays stripped.
        self.assertIn("helper-body", gate.shippable_rust(after))
        self.assertNotIn("helper-body", gate.shippable_rust(PROD))
        self.assertNotIn("seam-file-srv", gate.shippable_rust(after))
        self.assertTrue(gate.should_publish({"src/lib.rs": PROD}, {"src/lib.rs": after}))

    def test_real_function_change_publishes(self) -> None:
        after = PROD.replace("fn shipped() -> i32 { 1 }", "fn shipped() -> i32 { 2 }")
        self.assertTrue(gate.should_publish({"src/lib.rs": PROD}, {"src/lib.rs": after}))

    def test_docs_readme_workflow_fixtures_do_not_publish(self) -> None:
        before = {
            "README.md": "old",
            "docs/guide.md": "old",
            "README": "old",
            ".github/workflows/release.yml": "old",
            "crates/app/src/fixtures/sample.json": "old",
            "crates/app/tests/link.rs": "fn changed_test() {}",
        }
        after = {path: "new" for path in before}
        self.assertFalse(gate.should_publish(before, after))

    def test_workflow_only_does_not_publish(self) -> None:
        self.assertFalse(
            gate.should_publish(
                {".github/workflows/ci.yml": "name: CI\n"},
                {".github/workflows/ci.yml": "name: CI changed\n"},
            )
        )

    def test_mixed_docs_and_code_publishes(self) -> None:
        before = {"README.md": "docs", "src/lib.rs": "fn shipped() {}\n"}
        after = {"README.md": "edited", "src/lib.rs": "fn shipped() { let _ = 1; }\n"}
        self.assertTrue(gate.should_publish(before, after))

    def test_gate_tooling_does_not_publish_updater_script_does(self) -> None:
        self.assertFalse(
            gate.should_publish(
                {"scripts/test.sh": "old", "scripts/publish_gate.py": "old"},
                {"scripts/test.sh": "new", "scripts/publish_gate.py": "new"},
            )
        )
        self.assertTrue(
            gate.should_publish(
                {"scripts/make_latest_json.py": "old"},
                {"scripts/make_latest_json.py": "new"},
            )
        )

    def test_unparseable_rust_publishes(self) -> None:
        self.assertTrue(
            gate.should_publish(
                {"src/lib.rs": "fn shipped() {}\n"},
                {"src/lib.rs": "this is not rust {{{"},
            )
        )

    def test_dependency_version_publishes(self) -> None:
        before = {
            "Cargo.toml": _manifest('foo = "1"\n', 'bar = "1"\n'),
            "Cargo.lock": _lock(),
        }
        after = {
            "Cargo.toml": _manifest('foo = "2"\n', 'bar = "1"\n'),
            "Cargo.lock": _lock(foo="2.0.0", foo_sum="ccc"),
        }
        self.assertTrue(gate.should_publish(before, after))

    def test_dev_dependency_only_does_not_publish(self) -> None:
        before = {
            "Cargo.toml": _manifest('foo = "1"\n', 'bar = "1"\n'),
            "Cargo.lock": _lock(),
        }
        after = {
            "Cargo.toml": _manifest('foo = "1"\n', 'bar = "2"\n'),
            "Cargo.lock": _lock(bar="2.0.0", bar_sum="ddd"),
        }
        self.assertFalse(gate.should_publish(before, after))

    def test_target_dev_dependency_does_not_publish(self) -> None:
        before = {
            "Cargo.toml": _manifest('foo = "1"\n', target_dev='baz = "1"\n'),
            "Cargo.lock": _lock(bar=None),
        }
        after = {
            "Cargo.toml": _manifest('foo = "1"\n', target_dev='baz = "2"\n'),
            "Cargo.lock": _lock(bar=None),
        }
        self.assertFalse(gate.should_publish(before, after))

    def test_build_dependency_publishes(self) -> None:
        before = {
            "Cargo.toml": _manifest('foo = "1"\n', build='builder = "1"\n'),
            "Cargo.lock": _lock(bar=None) + '''
[[package]]
name = "builder"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "eee"
''',
        }
        after = {
            "Cargo.toml": _manifest('foo = "1"\n', build='builder = "1"\n'),
            "Cargo.lock": _lock(bar=None) + '''
[[package]]
name = "builder"
version = "1.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "fff"
''',
        }
        # The manifest requirement is unchanged; the lock checksum must publish
        # because builder is a build-dependency, and app's lock edges only list
        # direct deps. Declare the edge explicitly.
        before["Cargo.lock"] = before["Cargo.lock"].replace(
            'dependencies = ["foo"]', 'dependencies = ["builder", "foo"]', 1
        )
        after["Cargo.lock"] = after["Cargo.lock"].replace(
            'dependencies = ["foo"]', 'dependencies = ["builder", "foo"]', 1
        )
        self.assertTrue(gate.should_publish(before, after))

    def test_lock_runtime_checksum_publishes(self) -> None:
        common = {"Cargo.toml": _manifest('foo = "1"\n')}
        before = {**common, "Cargo.lock": _lock(bar=None)}
        after = {**common, "Cargo.lock": _lock(bar=None, foo_sum="zzz")}
        self.assertTrue(gate.should_publish(before, after))

    def test_lock_dev_only_checksum_does_not_publish(self) -> None:
        common = {"Cargo.toml": _manifest('foo = "1"\n', 'bar = "1"\n')}
        before = {**common, "Cargo.lock": _lock()}
        after = {**common, "Cargo.lock": _lock(bar_sum="zzz")}
        self.assertFalse(gate.should_publish(before, after))

    def test_version_requirements(self) -> None:
        self.assertTrue(gate.version_matches("1.2.3", "1"))
        self.assertTrue(gate.version_matches("0.14.10", "0.14"))
        self.assertFalse(gate.version_matches("0.15.0", "0.14"))
        self.assertTrue(gate.version_matches("4.18.2", "=4.18.2"))
        self.assertFalse(gate.version_matches("4.18.3", "=4.18.2"))
        self.assertFalse(gate.version_matches("1.0.0-alpha", "1"))
        self.assertTrue(gate.version_matches("1.2.3", "~1.2"))
        self.assertFalse(gate.version_matches("1.3.0", "~1.2.3"))

    def test_repo_sources_strip_and_keep_production_strings(self) -> None:
        server = (ROOT / "crates/seam-core/src/link/server.rs").read_text(encoding="utf-8")
        stripped = gate.shippable_rust(server)
        self.assertIn("this phone is not connected", stripped)
        self.assertNotIn("seam-file-srv", stripped)
        self.assertNotIn("fn scratch", stripped)
        failures = []
        for path in ROOT.rglob("*.rs"):
            if "target" in path.parts:
                continue
            try:
                gate.shippable_rust(path.read_text(encoding="utf-8"))
            except gate.Uncertain as exc:
                failures.append(f"{path.relative_to(ROOT)}: {exc}")
        self.assertEqual(failures, [])

    def test_real_server_test_edit_does_not_publish_but_production_does(self) -> None:
        path = "crates/seam-core/src/link/server.rs"
        original = (ROOT / path).read_text(encoding="utf-8")
        test_edit = original.replace("seam-file-srv-{label}", "seam-file-srv-edited-{label}", 1)
        self.assertNotEqual(original, test_edit)
        self.assertFalse(gate.should_publish({path: original}, {path: test_edit}))
        prod_edit = original.replace(
            'Err("this phone is not connected".into())',
            'Err("this phone is offline".into())',
            1,
        )
        self.assertTrue(gate.should_publish({path: original}, {path: prod_edit}))

    def test_real_workspace_closure_includes_runtime_crates(self) -> None:
        files: dict[str, str | None] = {}
        for path in ROOT.rglob("Cargo.toml"):
            if "target" in path.parts:
                continue
            files[str(path.relative_to(ROOT))] = path.read_text(encoding="utf-8")
        files["Cargo.lock"] = (ROOT / "Cargo.lock").read_text(encoding="utf-8")
        rows = gate.lock_runtime_fingerprint(files)
        names = {(name, version) for name, version, _source, _sum in rows}
        self.assertIn(("hmac", "0.13.0"), names)
        self.assertIn(("sha2", "0.11.1"), names)
        self.assertIn(("rcgen", "0.14.10"), names)
        self.assertIn(("rand", "0.9.5"), names)
        self.assertGreater(len(rows), 400)


class GitDecisionTests(unittest.TestCase):
    def test_tag_comparison_and_manual_dispatch(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self._git(tmp, ["init"])
            self._git(tmp, ["config", "user.email", "gate@example.com"])
            self._git(tmp, ["config", "user.name", "gate"])
            lib = Path(tmp) / "src"
            lib.mkdir()
            (lib / "lib.rs").write_text(PROD, encoding="utf-8")
            (Path(tmp) / "README.md").write_text("docs\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "base"])
            self._git(tmp, ["tag", "v0.1.9"])
            # An older-looking tag created later must not win version sort.
            self._git(tmp, ["tag", "v0.1.2"])
            (lib / "lib.rs").write_text(
                PROD.replace("seam-file-srv", "edited"), encoding="utf-8"
            )
            (Path(tmp) / "README.md").write_text("docs changed\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "tests and docs"])
            self._git(tmp, ["tag", "v0.1.10"])
            (lib / "lib.rs").write_text(
                PROD.replace("seam-file-srv", "edited-again"), encoding="utf-8"
            )
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "still only tests"])
            self.assertEqual(gate.latest_release_tag(tmp), "v0.1.10")
            self.assertFalse(gate.decide_git(cwd=tmp))
            (lib / "lib.rs").write_text(
                PROD.replace("fn shipped() -> i32 { 1 }", "fn shipped() -> i32 { 9 }"),
                encoding="utf-8",
            )
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "real code"])
            self.assertTrue(gate.decide_git(cwd=tmp))
            env = os.environ.copy()
            env["GITHUB_EVENT_NAME"] = "workflow_dispatch"
            env.pop("GITHUB_OUTPUT", None)
            proc = subprocess.run(
                ["python3", str(ROOT / "scripts/publish_gate.py")],
                cwd=tmp,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn("publish=true", proc.stdout)

    def test_no_tag_publishes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            self._git(tmp, ["init"])
            self._git(tmp, ["config", "user.email", "gate@example.com"])
            self._git(tmp, ["config", "user.name", "gate"])
            (Path(tmp) / "README.md").write_text("only docs\n", encoding="utf-8")
            self._git(tmp, ["add", "."])
            self._git(tmp, ["commit", "-m", "docs"])
            self.assertTrue(gate.decide_git(cwd=tmp))

    @staticmethod
    def _git(cwd: str, args: list[str]) -> None:
        subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True)


if __name__ == "__main__":
    unittest.main()
