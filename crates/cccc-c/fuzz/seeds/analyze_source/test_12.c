
            char *dup(const char *s, int retry) {
                if (retry) {
                    return dup(s, 0);
                }
                return 0;
            }
        