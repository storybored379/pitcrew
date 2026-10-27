"""Standard-library checks of the disposable reference export boundary."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from adapter import export_reference


class ReferenceExportTests(unittest.TestCase):
    def git(self, root, *args):
        return subprocess.run(['git', '-C', str(root), *args], check=True,
                              capture_output=True, text=True).stdout.strip()

    def repository(self, root):
        self.git(root, 'init', '--quiet')
        self.git(root, 'config', 'user.name', 'Synthetic Test')
        self.git(root, 'config', 'user.email', 'fixture@users.noreply.github.com')
        (root / 'app.py').write_text('committed source\n')
        self.git(root, 'add', 'app.py')
        self.git(root, 'commit', '--quiet', '-m', 'synthetic reference')

    def test_exports_commit_without_copying_dirty_or_untracked_reference_data(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            reference, destination = root / 'reference', root / 'export'
            reference.mkdir()
            destination.mkdir()
            self.repository(reference)
            (reference / 'app.py').write_text('private dirty data\n')
            (reference / 'private.env').write_text('untracked data\n')
            before = self.git(reference, 'status', '--porcelain')
            commit = export_reference(reference, 'HEAD', destination)
            self.assertEqual(commit, self.git(reference, 'rev-parse', 'HEAD'))
            self.assertEqual((destination / 'app.py').read_text(), 'committed source\n')
            self.assertFalse((destination / 'private.env').exists())
            self.assertFalse((destination / '.git').exists())
            self.assertEqual(self.git(reference, 'status', '--porcelain'), before)
            self.assertEqual((reference / 'app.py').read_text(), 'private dirty data\n')

    def test_export_refuses_link_escaping_scratch(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            reference, destination = root / 'reference', root / 'export'
            reference.mkdir()
            destination.mkdir()
            self.repository(reference)
            os.symlink('../../outside', reference / 'escape')
            self.git(reference, 'add', 'escape')
            self.git(reference, 'commit', '--quiet', '-m', 'unsafe synthetic link')
            with self.assertRaises(ValueError):
                export_reference(reference, 'HEAD', destination)
            self.assertFalse((root / 'outside').exists())


if __name__ == '__main__':
    unittest.main()
