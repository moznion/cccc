
            class Foo {
                const int &value() const {
                    if (a) { }
                    return a;
                }
            };
            int &Foo::at(int i) {
                if (i) { }
                return a;
            }
        