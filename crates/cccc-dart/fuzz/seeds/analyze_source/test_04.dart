
                void host() {
                  final callback = (bool a, bool b) {
                    if (a && b) {
                      return;
                    }
                  };
                  callback(true, false);
                }
            