
            struct S {
                operator bool() const {
                    if (a) { }
                    return a;
                }
                explicit operator const char *() {
                    if (a) { }
                    return "";
                }
            };
            S::operator int() {
                if (a) { }
                return 0;
            }
        