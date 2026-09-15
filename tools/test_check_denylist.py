import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

SCRIPT = Path(__file__).with_name('check_denylist.py')


class DenylistCheckTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        base = Path(self.temp.name)
        self.repo = base / 'repo'
        self.repo.mkdir()
        self.denylist = base / 'denylist.txt'
        self.denylist.write_text('# customer terms\nacmecorp\n', encoding='utf-8')
        self.git('init', '-q')
        self.git('config', 'user.name', 'Test')
        self.git('config', 'user.email', 'test@example.com')
        self.git('config', 'commit.gpgsign', 'false')

    def git(self, *args):
        subprocess.run(['git', '-C', str(self.repo), *args], check=True, capture_output=True)

    def write(self, rel, text):
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding='utf-8')

    def commit_all(self):
        self.git('add', '-A')
        self.git('commit', '-q', '-m', 'fixture')

    def check(self, *args, env_denylist=None, use_flag=True):
        env = dict(os.environ, PYTHONIOENCODING='cp1252')
        env.pop('ENGRAM_DENYLIST', None)
        if env_denylist is not None:
            env['ENGRAM_DENYLIST'] = str(env_denylist)
        flag = ['--denylist', str(self.denylist)] if use_flag else []
        return subprocess.run(
            [sys.executable, str(SCRIPT), '--repo', str(self.repo), *flag, *args],
            env=env, capture_output=True, text=True, timeout=30)

    def test_staged_addition_in_scope_is_rejected_with_its_line(self):
        self.write('crates/app.rs', 'fn a() {}\nfn b() {}\nfn c() {}\n')
        self.commit_all()
        self.write('crates/app.rs', 'fn a() {}\nfn b() {}\nfn c() {}\n// acmecorp live miss\n')
        self.git('add', '-A')

        result = self.check('--staged', 'crates')

        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('crates/app.rs:4', result.stdout)

    def test_match_is_case_insensitive(self):
        self.write('crates/app.rs', 'let org = "AcmeCorp";\n')
        self.git('add', '-A')

        result = self.check('--staged', 'crates')

        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_clean_staged_change_passes(self):
        self.write('crates/app.rs', 'let org = "exampleorg";\n')
        self.git('add', '-A')

        result = self.check('--staged', 'crates')

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_existing_lines_do_not_block_an_unrelated_staged_edit(self):
        self.write('crates/app.rs', '// acmecorp legacy comment\nfn a() {}\n')
        self.commit_all()
        self.write('crates/app.rs', '// acmecorp legacy comment\nfn a() {}\nfn b() {}\n')
        self.git('add', '-A')

        result = self.check('--staged', 'crates')

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_paths_outside_scope_are_ignored(self):
        self.write('docs/notes.md', 'acmecorp evidence\n')
        self.git('add', '-A')

        result = self.check('--staged', 'crates')

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_tree_mode_reports_committed_occurrences(self):
        self.write('crates/app.rs', 'fn a() {}\n// acmecorp legacy comment\n')
        self.write('docs/notes.md', 'acmecorp evidence\n')
        self.commit_all()

        result = self.check('--tree', 'crates')

        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('crates/app.rs:2', result.stdout)
        self.assertNotIn('docs/notes.md', result.stdout)

    def test_denylist_path_can_come_from_environment(self):
        self.write('crates/app.rs', 'acmecorp\n')
        self.git('add', '-A')

        result = self.check('--staged', 'crates', env_denylist=self.denylist, use_flag=False)

        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)

    def test_missing_denylist_warns_and_passes(self):
        self.write('crates/app.rs', 'acmecorp\n')
        self.git('add', '-A')
        self.denylist.unlink()

        result = self.check('--staged', 'crates')

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn('denylist', result.stderr.lower())

    def test_comment_and_blank_lines_are_not_patterns(self):
        self.denylist.write_text('# customer terms\n\n   \nacmecorp\n', encoding='utf-8')
        self.write('crates/app.rs', '// list customer terms here\nfn a() {}\n')
        self.git('add', '-A')

        result = self.check('--staged', 'crates')

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_invalid_pattern_fails_loudly_naming_its_line(self):
        self.denylist.write_text('acmecorp\nbroken(\n', encoding='utf-8')
        self.write('crates/app.rs', 'fn a() {}\n')
        self.git('add', '-A')

        result = self.check('--staged', 'crates')

        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn(':2', result.stderr)


if __name__ == '__main__':
    unittest.main()
