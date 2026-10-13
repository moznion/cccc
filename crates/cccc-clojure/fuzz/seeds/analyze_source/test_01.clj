
            (defn fact
              "computes n!"
              {:added "1.0"}
              [n]
              (if (< n 2) 1 (* n (fact (- n 1)))))
        