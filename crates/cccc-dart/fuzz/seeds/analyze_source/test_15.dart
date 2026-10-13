
                String f(Object value) => switch (value) {
                  [int first && > 0, int second && < 10] => 'matched',
                  _ => 'other',
                };
            