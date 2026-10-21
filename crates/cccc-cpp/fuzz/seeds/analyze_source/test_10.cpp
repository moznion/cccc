
            class Foo {
                Foo operator+(const Foo &other) {
                    if (true) { }
                    return *this;
                }
            };
        