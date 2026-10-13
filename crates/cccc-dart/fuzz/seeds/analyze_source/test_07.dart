
            bool f<T>(T value, List<T> values) {
              outer: while (true) {
                if (values.where<T>((item) => item == value).isNotEmpty &&
                    1 + 2 == 3) {
                  break outer;
                }
              }
              return false;
            }
        