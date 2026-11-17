"""Standalone bounded local-worker execution for Ringo.

This package is deliberately independent of the shell, the tool gateway and the
provider registry. It consumes a versioned assignment file and returns a
versioned result file; applying a candidate to the owner's working tree is not
part of this module.
"""

from .schemas import ASSIGNMENT_SCHEMA_VERSION, RESULT_SCHEMA_VERSION

__all__ = ["ASSIGNMENT_SCHEMA_VERSION", "RESULT_SCHEMA_VERSION"]
