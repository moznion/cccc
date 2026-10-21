
                final topCallback = () {};
                class C {
                  final fieldCallback = () {};
                }
                void host() {
                  final callback = ((bool value) {
                    if (value) {}
                  });
                  use(() { if (true) {} });
                }
                void use(Function callback) {}
            