
            (define (classify x)
              (match x
                ((list a b) 'pair)
                ((? number?) 'num)
                (_ 'other)))
        