
            Map<String, int> f(String? key, int? value) => {
              ?key: 1,
              'value': ?value,
              ?key: ?value,
            };
        