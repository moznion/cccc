
            (define (count n)
              (let loop ((i 0))
                (if (< i n)
                    (loop (+ i 1))
                    i)))
        