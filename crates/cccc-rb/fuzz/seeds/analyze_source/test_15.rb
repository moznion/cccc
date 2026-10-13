
            class S
              def walk(n)
                if n == 0
                  0
                else
                  self.walk(n - 1)
                end
              end
            end
        