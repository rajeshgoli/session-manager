"""Pin attribution, inclusive sample accounting, and wait exclusions."""

import pathlib
import tempfile
import unittest
from prof import profile


class ProfileTest(unittest.TestCase):
    def test_busy_and_mutex_attribution(self):
        graph = '''Call graph:
    20 Thread_1
    + 20 sm_server::run_background_pass::h123
    +   20 sm_server::sessions::retry::h123
    +     5 sm_server::sessions::lookup::h123
    +       3 __psynch_mutexwait
    +       2 clone
    +     10 __psynch_cvwait
    +     4 work
    7 Thread_2
    + 7 tokio::worker
    +   7 poll
Total number in stack (recursive counted multiple, when >=5):
'''
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'sample.txt'
            path.write_text(graph)
            result = profile(path)
        self.assertEqual(result['busy'], 7)
        self.assertEqual(result['top'], {'sm_server::sessions::retry': 7})
        self.assertEqual(result['deepest'], {
            'sm_server::sessions::lookup': 2, 'sm_server::sessions::retry': 5})
        self.assertEqual(result['mutex'], {'sm_server::sessions::lookup': 3})


if __name__ == '__main__':
    unittest.main()
