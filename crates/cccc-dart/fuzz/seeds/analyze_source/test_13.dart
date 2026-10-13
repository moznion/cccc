
            int f(Object value, bool enabled, bool ready) {
              switch (value) {
                case int parsed && > 0 when enabled && ready:
                  return parsed;
                default:
                  return 0;
              }
            }
        