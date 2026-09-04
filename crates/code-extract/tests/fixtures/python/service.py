"""Synthetic service module used by the extraction tests."""

import os
import pkg.mod_a
from pkg.sub import deep
from . import sibling
from ..shared import util

CAP = 12


class Store:
    """Keeps records in memory."""

    def put(self, key):
        """Store one value."""
        self.flush()
        return deep.load(key)

    def flush(self):
        pass


def main():
    """Entry point."""
    s = Store()
    s.put("a")
    return sibling.tick() + util.now() + os.getpid()
