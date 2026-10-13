
            bool f(Object value) {
              if (value case (int a && > 0) || (int b && < -10)) {
                return true;
              }
              return false;
            }
        