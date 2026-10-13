
            def f(value)
              value&.property
              value&.child&.run
            end
        