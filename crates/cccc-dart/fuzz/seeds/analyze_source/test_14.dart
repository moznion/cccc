
            int records(Object value) {
              switch (value) {
                case (int first && > 0, int second && < 10):
                  return first + second;
                default:
                  return 0;
              }
            }

            int map(Object value) {
              switch (value) {
                case {'left': int left && > 0, 'right': int right && < 10}:
                  return left + right;
                default:
                  return 0;
              }
            }
        