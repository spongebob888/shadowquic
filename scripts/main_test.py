import sys
import os

# Ensure test_suite is in the import path
sys.path.append(os.path.join(os.path.dirname(__file__), "test_suite"))

import basic_test
import test_detection

# The imported modules will execute their tests on import