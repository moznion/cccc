
            interface I {
                default int f(boolean x) {
                    if (x) {
                        return 1;
                    }
                    return 0;
                }
            }
        