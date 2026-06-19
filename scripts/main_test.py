import sys
import os

# Ensure test_suite is in the import path
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "test_suite"))

import basic_test
import test_detection
import test_tproxy
