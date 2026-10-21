
            List<int> f(int value, List<int> values) => [
              if (value > 0) value else 0,
              for (final item in values)
                if (item.isEven) item,
            ];
        